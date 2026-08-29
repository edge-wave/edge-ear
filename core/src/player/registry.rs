//! Sounds an application has registered, ready to play. Decoding
//! happens once at registration, so playing is only a copy, and
//! registering while running rebuilds no pipeline.

use std::collections::HashMap;
use std::path::PathBuf;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::capture::Samples;
use crate::config::{AudioFormat, SampleType};
use crate::error::{Error, Result};
use crate::player::envelope;

pub type SoundId = String;

/// Where a sound's audio comes from. A Rust enum here; the C API turns
/// it into a tag plus a union in its own crate.
#[derive(Debug, Clone, PartialEq)]
pub enum SoundSource {
    File {
        path: PathBuf,
    },
    Pcm {
        /// Raw audio carries no header, so the caller says what it is.
        /// The type travels with the samples and cannot be misdeclared.
        data: Samples,
        sample_rate: u32,
        channels: u16,
    },
}

/// A decoded sound, shaped and ready for the speaker.
#[derive(Debug, Clone)]
pub struct RegisteredSound {
    pub id: SoundId,
    /// Interleaved, already at the output format, already shaped.
    pub samples: Vec<i16>,
    /// What the samples were converted to when the sound was
    /// registered. Kept so the conversion can be checked.
    #[allow(dead_code, reason = "records what conversion produced")]
    pub format: AudioFormat,
    pub volume: f32,
}

impl RegisteredSound {
    #[cfg(test)]
    pub fn duration(&self) -> std::time::Duration {
        let frames = self.samples.len() / self.format.channels.max(1) as usize;
        std::time::Duration::from_secs_f64(frames as f64 / self.format.sample_rate as f64)
    }
}

#[derive(Debug, Default)]
pub struct Registry {
    sounds: HashMap<SoundId, RegisteredSound>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode, shape, and store. Replaces anything under the same id.
    pub fn register(
        &mut self,
        id: SoundId,
        source: SoundSource,
        volume: f32,
        output: AudioFormat,
    ) -> Result<()> {
        if !(0.0..=1.0).contains(&volume) || volume.is_nan() {
            return Err(Error::InvalidValue {
                setting: "volume",
                expected: "between 0.0 and 1.0".to_string(),
                got: volume.to_string(),
            });
        }

        let (raw, source_format) = match source {
            SoundSource::File { path } => {
                let (samples, format) = decode_file(&path)?;
                (Samples::I16(samples), format)
            }
            SoundSource::Pcm {
                data,
                sample_rate,
                channels,
            } => {
                let format = AudioFormat::new(sample_rate, channels, data.sample_type());
                (data, format)
            }
        };

        let at_output = to_output(&raw, source_format, output)?;
        let shaped = envelope::shape(at_output, output.sample_rate, output.channels);

        self.sounds.insert(
            id.clone(),
            RegisteredSound {
                id,
                samples: shaped,
                format: output,
                volume,
            },
        );
        Ok(())
    }

    /// Forget a sound and release the audio it held. Without this an
    /// application registering each spoken reply would grow for ever.
    pub fn unregister(&mut self, id: &str) -> Result<()> {
        self.sounds
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| Error::UnknownSound(id.to_string()))
    }

    pub fn get(&self, id: &str) -> Result<&RegisteredSound> {
        self.sounds
            .get(id)
            .ok_or_else(|| Error::UnknownSound(id.to_string()))
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.sounds.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.sounds.is_empty()
    }

    #[cfg(test)]
    /// Samples held across every registered sound. Used by the test
    /// that checks memory stays flat over repeated register cycles.
    pub fn total_samples(&self) -> usize {
        self.sounds.values().map(|s| s.samples.len()).sum()
    }

    pub fn clear(&mut self) {
        self.sounds.clear();
    }
}

/// Read a sound file into interleaved 16-bit samples.
pub(crate) fn decode_file(path: &PathBuf) -> Result<(Vec<i16>, AudioFormat)> {
    if !path.exists() {
        return Err(Error::ModelNotFound { path: path.clone() });
    }

    let file = std::fs::File::open(path).map_err(|e| Error::ModelUnreadable {
        path: path.clone(),
        reason: e.to_string(),
    })?;

    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }

    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut reader = symphonia::default::get_probe()
        .probe(
            &hint,
            stream,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| Error::ModelInvalid {
            path: path.clone(),
            reason: format!("not a sound file this library can read: {e}"),
        })?;

    let track = reader
        .tracks()
        .iter()
        .find(|t| t.codec_params.is_some())
        .ok_or_else(|| Error::ModelInvalid {
            path: path.clone(),
            reason: "the file holds no audio track".to_string(),
        })?;
    let track_id = track.id;

    let audio_params = match track.codec_params.as_ref() {
        Some(symphonia::core::codecs::CodecParameters::Audio(p)) => p.clone(),
        _ => {
            return Err(Error::ModelInvalid {
                path: path.clone(),
                reason: "the only track is not audio".to_string(),
            });
        }
    };

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|e| Error::ModelInvalid {
            path: path.clone(),
            reason: format!("no decoder for this file: {e}"),
        })?;

    let mut samples: Vec<i16> = Vec::new();
    let mut format: Option<AudioFormat> = None;

    while let Some(packet) = reader.next_packet().map_err(|e| Error::ModelUnreadable {
        path: path.clone(),
        reason: e.to_string(),
    })? {
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // A damaged packet is skipped rather than failing the whole
            // file, which is what a player would do.
            Err(_) => continue,
        };
        if format.is_none() {
            format = Some(spec_of(&decoded));
        }
        let mut block: Vec<i16> = Vec::new();
        decoded.copy_to_vec_interleaved(&mut block);
        samples.append(&mut block);
    }

    let format = format.ok_or_else(|| Error::ModelInvalid {
        path: path.clone(),
        reason: "the file decoded to no audio at all".to_string(),
    })?;

    Ok((samples, format))
}

fn spec_of(decoded: &GenericAudioBufferRef<'_>) -> AudioFormat {
    let spec = decoded.spec();
    AudioFormat::new(spec.rate(), spec.channels().count() as u16, SampleType::I16)
}

/// Bring a decoded sound to the format the speaker is running at.
/// Bring a sound to what the speaker takes. Sounds are kept as 16-bit
/// whatever they arrived as, so this is where a 32-bit source lands.
fn to_output(raw: &Samples, from: AudioFormat, to: AudioFormat) -> Result<Vec<i16>> {
    let wanted = AudioFormat::new(to.sample_rate, to.channels, SampleType::I16);
    let mut converter = crate::capture::convert::Converter::new(from, wanted)?;

    let mut out = as_i16(converter.convert(raw)?);
    // Take whatever the resampler was still holding, so the end of a
    // short sound is not clipped off.
    out.append(&mut as_i16(converter.flush()?));
    Ok(out)
}

fn as_i16(samples: Samples) -> Vec<i16> {
    match samples {
        Samples::I16(v) => v,
        Samples::F32(v) => v
            .iter()
            .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output() -> AudioFormat {
        AudioFormat::mono_16k()
    }

    fn pcm(samples: usize) -> SoundSource {
        SoundSource::Pcm {
            data: Samples::I16(vec![8_000; samples]),
            sample_rate: 16_000,
            channels: 1,
        }
    }

    #[test]
    fn a_registered_sound_can_be_fetched_again() {
        let mut registry = Registry::new();
        registry
            .register("alert".into(), pcm(1600), 0.8, output())
            .unwrap();

        let sound = registry.get("alert").unwrap();
        assert_eq!(sound.id, "alert");
        assert_eq!(sound.volume, 0.8);
        assert!(!sound.samples.is_empty());
    }

    #[test]
    fn an_unknown_id_says_so() {
        let registry = Registry::new();
        let err = registry.get("nothing").expect_err("must fail");
        assert!(matches!(err, Error::UnknownSound(_)), "{err}");
    }

    #[test]
    fn registering_the_same_id_again_replaces_it() {
        let mut registry = Registry::new();
        registry
            .register("reply".into(), pcm(1600), 1.0, output())
            .unwrap();
        let first = registry.get("reply").unwrap().samples.len();

        registry
            .register("reply".into(), pcm(3200), 1.0, output())
            .unwrap();
        let second = registry.get("reply").unwrap().samples.len();

        assert!(second > first, "the newer audio replaced the older");
        assert_eq!(registry.len(), 1, "it did not become a second entry");
    }

    #[test]
    fn unregistering_releases_what_it_held() {
        let mut registry = Registry::new();
        registry
            .register("reply".into(), pcm(16_000), 1.0, output())
            .unwrap();
        assert!(registry.total_samples() > 16_000);

        registry.unregister("reply").unwrap();
        assert_eq!(registry.total_samples(), 0);
        assert!(registry.is_empty());
    }

    #[test]
    fn unregistering_something_that_was_never_there_says_so() {
        let mut registry = Registry::new();
        let err = registry.unregister("ghost").expect_err("must fail");
        assert!(matches!(err, Error::UnknownSound(_)), "{err}");
    }

    #[test]
    fn many_register_and_unregister_cycles_do_not_pile_up() {
        let mut registry = Registry::new();
        for n in 0..200 {
            let id = format!("reply-{n}");
            registry
                .register(id.clone(), pcm(16_000), 1.0, output())
                .unwrap();
            registry.unregister(&id).unwrap();
        }
        assert_eq!(registry.total_samples(), 0);
        assert!(registry.is_empty());
    }

    #[test]
    fn a_volume_outside_the_range_is_refused() {
        let mut registry = Registry::new();
        let err = registry
            .register("alert".into(), pcm(1600), 2.0, output())
            .expect_err("must fail");
        assert!(matches!(err, Error::InvalidValue { .. }), "{err}");
        assert!(registry.is_empty(), "nothing was stored");
    }

    #[test]
    fn raw_audio_at_another_rate_is_brought_to_the_output_rate() {
        let mut registry = Registry::new();
        registry
            .register(
                "reply".into(),
                SoundSource::Pcm {
                    // Reply audio often arrives at 24 kHz.
                    data: Samples::I16(vec![4_000; 24_000]),
                    sample_rate: 24_000,
                    channels: 1,
                },
                1.0,
                output(),
            )
            .unwrap();

        let sound = registry.get("reply").unwrap();
        assert_eq!(sound.format.sample_rate, 16_000);
        // One second in, so about one second out, plus the padding.
        let seconds = sound.duration().as_secs_f64();
        assert!((0.9..=1.4).contains(&seconds), "got {seconds} seconds");
    }

    #[test]
    fn a_missing_file_is_reported_as_missing() {
        let mut registry = Registry::new();
        let err = registry
            .register(
                "alert".into(),
                SoundSource::File {
                    path: "/no/such/sound.wav".into(),
                },
                1.0,
                output(),
            )
            .expect_err("must fail");
        assert!(matches!(err, Error::ModelNotFound { .. }), "{err}");
    }
}
