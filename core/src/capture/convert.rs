//! Turning what the device produces into what each consumer asked for.
//!
//! The device is read once, in its own format. Conversion happens per
//! requested format, not per consumer, so two consumers wanting the
//! same thing share the work.

use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Resampler};

use crate::capture::Samples;
use crate::config::{AudioFormat, SampleType};
use crate::error::{Error, Result};

/// Frames the resampler takes at a time. Small enough to keep added
/// delay low, large enough that the transform is worth doing.
const RESAMPLE_CHUNK: usize = 1024;

/// Converts one device format into one consumer format.
///
/// Resampling carries state between calls, so a converter belongs to a
/// stream and must not be reused across a stop and start.
pub struct Converter {
    from: AudioFormat,
    to: AudioFormat,
    resampler: Option<Fft<f32>>,
    out_max: usize,
    /// Mono f32 input waiting for a full resampler chunk.
    pending: Vec<f32>,
}

impl Converter {
    pub fn new(from: AudioFormat, to: AudioFormat) -> Result<Self> {
        let resampler = if from.sample_rate == to.sample_rate {
            None
        } else {
            Some(
                Fft::<f32>::new(
                    from.sample_rate as usize,
                    to.sample_rate as usize,
                    RESAMPLE_CHUNK,
                    1,
                    FixedSync::Input,
                )
                .map_err(|e| Error::Conversion {
                    reason: format!(
                        "cannot resample {} Hz to {} Hz: {e}",
                        from.sample_rate, to.sample_rate
                    ),
                })?,
            )
        };

        let out_max = resampler.as_ref().map_or(0, |r| r.output_frames_max());

        Ok(Self {
            from,
            to,
            resampler,
            out_max,
            pending: Vec::new(),
        })
    }

    /// True when the device already gives exactly what is wanted.
    pub fn is_pass_through(&self) -> bool {
        self.from == self.to
    }

    pub fn from(&self) -> AudioFormat {
        self.from
    }

    pub fn to(&self) -> AudioFormat {
        self.to
    }

    /// Convert one block. May return fewer samples than went in, or
    /// none at all, while the resampler fills its next chunk.
    pub fn convert(&mut self, input: &Samples) -> Result<Samples> {
        if self.is_pass_through() {
            return Ok(input.clone());
        }

        let mono = to_mono_f32(input, self.from.channels);
        let resampled = self.resample(mono)?;
        Ok(from_mono_f32(
            &resampled,
            self.to.channels,
            self.to.sample_type,
        ))
    }

    /// Anything the resampler is still holding. Used when a stream ends
    /// so the tail is not lost.
    pub fn flush(&mut self) -> Result<Samples> {
        if self.is_pass_through() || self.pending.is_empty() {
            return Ok(empty_of(self.to.sample_type));
        }
        let padding = RESAMPLE_CHUNK - (self.pending.len() % RESAMPLE_CHUNK);
        self.pending.extend(std::iter::repeat_n(0.0, padding));
        let tail = self.resample(Vec::new())?;
        Ok(from_mono_f32(&tail, self.to.channels, self.to.sample_type))
    }

    fn resample(&mut self, mono: Vec<f32>) -> Result<Vec<f32>> {
        let Some(resampler) = self.resampler.as_mut() else {
            return Ok(mono);
        };

        self.pending.extend_from_slice(&mono);
        let mut out = Vec::new();

        while self.pending.len() >= RESAMPLE_CHUNK {
            let block: Vec<f32> = self.pending.drain(..RESAMPLE_CHUNK).collect();
            let input_data = [block];
            let input =
                SequentialSliceOfVecs::new(&input_data[..], 1, RESAMPLE_CHUNK).map_err(|e| {
                    Error::Conversion {
                        reason: format!("input buffer rejected: {e}"),
                    }
                })?;

            let mut output_data = [vec![0.0f32; self.out_max]];
            let mut output = SequentialSliceOfVecs::new_mut(&mut output_data[..], 1, self.out_max)
                .map_err(|e| Error::Conversion {
                    reason: format!("output buffer rejected: {e}"),
                })?;

            let (_, produced) = resampler
                .process_into_buffer(&input, &mut output, None)
                .map_err(|e| Error::Conversion {
                    reason: format!("resampling failed: {e}"),
                })?;

            out.extend_from_slice(&output_data[0][..produced]);
        }

        Ok(out)
    }
}

fn empty_of(sample_type: SampleType) -> Samples {
    match sample_type {
        SampleType::I16 => Samples::I16(Vec::new()),
        SampleType::F32 => Samples::F32(Vec::new()),
    }
}

/// Flatten to one channel of f32 in [-1, 1]. Several channels are
/// averaged rather than dropped, so nothing said on one side is lost.
fn to_mono_f32(input: &Samples, channels: u16) -> Vec<f32> {
    let channels = channels.max(1) as usize;
    match input {
        Samples::I16(v) => v
            .chunks(channels)
            .map(|frame| {
                let sum: f32 = frame.iter().map(|s| *s as f32 / 32768.0).sum();
                sum / frame.len() as f32
            })
            .collect(),
        Samples::F32(v) => v
            .chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
            .collect(),
    }
}

/// Back to the consumer's shape. One channel stays as it is; two get
/// the same audio on both sides.
fn from_mono_f32(mono: &[f32], channels: u16, sample_type: SampleType) -> Samples {
    let repeat = channels.max(1) as usize;
    match sample_type {
        SampleType::I16 => {
            let mut out = Vec::with_capacity(mono.len() * repeat);
            for s in mono {
                let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                for _ in 0..repeat {
                    out.push(v);
                }
            }
            Samples::I16(out)
        }
        SampleType::F32 => {
            let mut out = Vec::with_capacity(mono.len() * repeat);
            for s in mono {
                for _ in 0..repeat {
                    out.push(*s);
                }
            }
            Samples::F32(out)
        }
    }
}

/// Gathers converted audio into the exact frame size a model needs.
///
/// The two detectors want different frame sizes from the same device
/// blocks, so each keeps its own accumulator.
pub struct FrameAccumulator {
    /// `None` passes everything straight through, which is what the
    /// read path wants.
    frame_samples: Option<usize>,
    i16_buf: Vec<i16>,
    f32_buf: Vec<f32>,
}

impl FrameAccumulator {
    pub fn new(frame_samples: Option<usize>) -> Self {
        Self {
            frame_samples,
            i16_buf: Vec::new(),
            f32_buf: Vec::new(),
        }
    }

    /// Add a block and take out every complete frame it made.
    pub fn push(&mut self, samples: Samples) -> Vec<Samples> {
        let Some(frame) = self.frame_samples else {
            return if samples.is_empty() {
                Vec::new()
            } else {
                vec![samples]
            };
        };

        let mut out = Vec::new();
        match samples {
            Samples::I16(v) => {
                self.i16_buf.extend_from_slice(&v);
                while self.i16_buf.len() >= frame {
                    out.push(Samples::I16(self.i16_buf.drain(..frame).collect()));
                }
            }
            Samples::F32(v) => {
                self.f32_buf.extend_from_slice(&v);
                while self.f32_buf.len() >= frame {
                    out.push(Samples::F32(self.f32_buf.drain(..frame).collect()));
                }
            }
        }
        out
    }

    /// Samples held back, waiting for a frame to fill.
    pub fn held(&self) -> usize {
        self.i16_buf.len() + self.f32_buf.len()
    }

    pub fn clear(&mut self) {
        self.i16_buf.clear();
        self.f32_buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i16_format(rate: u32, channels: u16) -> AudioFormat {
        AudioFormat::new(rate, channels, SampleType::I16)
    }

    #[test]
    fn matching_formats_pass_straight_through() {
        let format = AudioFormat::mono_16k();
        let mut c = Converter::new(format, format).unwrap();
        assert!(c.is_pass_through());

        let input = Samples::I16(vec![1, 2, 3, 4]);
        assert_eq!(c.convert(&input).unwrap(), input);
    }

    #[test]
    fn stereo_is_averaged_into_mono() {
        let mut c = Converter::new(i16_format(16_000, 2), i16_format(16_000, 1)).unwrap();
        // Two frames: (100, 300) and (-200, -400).
        let out = c
            .convert(&Samples::I16(vec![100, 300, -200, -400]))
            .unwrap();
        let got = out.as_i16().unwrap();
        assert_eq!(got.len(), 2);
        assert!((got[0] - 200).abs() <= 1, "got {got:?}");
        assert!((got[1] + 300).abs() <= 1, "got {got:?}");
    }

    #[test]
    fn mono_is_copied_to_both_sides_for_stereo() {
        let mut c = Converter::new(i16_format(16_000, 1), i16_format(16_000, 2)).unwrap();
        let out = c.convert(&Samples::I16(vec![500, -500])).unwrap();
        assert_eq!(out.as_i16().unwrap(), &[500, 500, -500, -500]);
    }

    #[test]
    fn sample_type_changes_without_changing_the_sound() {
        let from = i16_format(16_000, 1);
        let to = AudioFormat::new(16_000, 1, SampleType::F32);
        let mut c = Converter::new(from, to).unwrap();
        let out = c.convert(&Samples::I16(vec![16384, -16384])).unwrap();
        let got = out.as_f32().unwrap();
        assert!((got[0] - 0.5).abs() < 0.01, "got {got:?}");
        assert!((got[1] + 0.5).abs() < 0.01, "got {got:?}");
    }

    #[test]
    fn resampling_produces_roughly_the_expected_rate_ratio() {
        // 44.1 kHz in, 16 kHz out: a bit over a third as many samples.
        let mut c = Converter::new(i16_format(44_100, 1), i16_format(16_000, 1)).unwrap();

        let mut produced = 0usize;
        let block = Samples::I16(vec![0; 4410]); // 100 ms at 44.1 kHz
        for _ in 0..10 {
            produced += c.convert(&block).unwrap().len();
        }

        // One second in, so about 16000 out. The resampler holds a
        // little back, so allow a margin rather than demanding exact.
        assert!(
            (15_000..=16_500).contains(&produced),
            "expected about 16000 samples, got {produced}"
        );
    }

    #[test]
    fn a_device_rate_the_resampler_cannot_take_is_reported() {
        let err = Converter::new(i16_format(0, 1), i16_format(16_000, 1))
            .err()
            .expect("a zero sample rate cannot be resampled");
        assert!(matches!(err, Error::Conversion { .. }), "{err}");
    }

    #[test]
    fn the_accumulator_hands_out_exact_frames() {
        let mut acc = FrameAccumulator::new(Some(512));

        // Not enough yet.
        assert!(acc.push(Samples::I16(vec![0; 300])).is_empty());
        assert_eq!(acc.held(), 300);

        // Now two full frames, with the remainder held back.
        let frames = acc.push(Samples::I16(vec![0; 800]));
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| f.len() == 512));
        assert_eq!(acc.held(), 1100 - 1024);
    }

    #[test]
    fn the_two_detectors_get_their_own_frame_sizes_from_one_block() {
        let mut wake = FrameAccumulator::new(Some(1280));
        let mut speech = FrameAccumulator::new(Some(512));

        let block = Samples::I16(vec![0; 2560]);
        assert_eq!(wake.push(block.clone()).len(), 2);
        assert_eq!(speech.push(block).len(), 5);
    }

    #[test]
    fn no_frame_size_means_pass_through() {
        let mut acc = FrameAccumulator::new(None);
        let out = acc.push(Samples::I16(vec![0; 37]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 37);
        assert_eq!(acc.held(), 0);
    }
}
