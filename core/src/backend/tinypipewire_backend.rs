//! The PipeWire audio backend using tinypipewire-rs.
//!
//! Audio capture and playback are bridged between PipeWire's real-time thread loop
//! and edge-ear's ring queues so neither waits on the other.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tinypipewire::{AudioConfig, Routing, SampleFormat, Stream};

use super::playout::{Playout, guessed_ahead};
use super::{AudioBackend, DeviceInfo, FormatRequest, InputStream, OutputStream, SupportedFormat};
use crate::backend::device_of;
use crate::capture::Samples;
use crate::capture::ring::{LossReport, Ring};
use crate::config::{AudioFormat, Device, SampleType};
use crate::error::{Error, Result};

/// Blocks the device queue holds before the oldest goes. The capture
/// thread drains this immediately, so it only fills if something above
/// has stalled, and dropping is better than growing.
const DEVICE_QUEUE_BLOCKS: usize = 32;

/// An audio backend backed by PipeWire through `tinypipewire-rs`.
pub struct TinypipewireBackend {}

impl TinypipewireBackend {
    /// Creates a new `TinypipewireBackend`.
    pub fn new() -> Result<Self> {
        Ok(Self {})
    }

    /// Resolves a requested device string (by id or name) to a PipeWire target identifier.
    fn pick_device(&self, is_input: bool, name: Option<&str>) -> Result<Option<String>> {
        let Some(wanted) = name else {
            return Ok(None);
        };
        let devices = if is_input {
            self.input_devices()?
        } else {
            self.output_devices()?
        };
        let found = devices.iter().find(|d| d.id == wanted || d.name == wanted);
        match found {
            Some(d) => Ok(Some(d.id.clone())),
            None => Err(Error::NoDevice(device_of(is_input))),
        }
    }
}

impl Default for TinypipewireBackend {
    fn default() -> Self {
        Self::new().expect("tinypipewire backend creation")
    }
}

/// Formats supported across PipeWire streams.
fn supported_formats() -> Vec<SupportedFormat> {
    vec![
        SupportedFormat {
            channels: 1,
            min_sample_rate: 8_000,
            max_sample_rate: 192_000,
            sample_type: SampleType::I16,
        },
        SupportedFormat {
            channels: 2,
            min_sample_rate: 8_000,
            max_sample_rate: 192_000,
            sample_type: SampleType::I16,
        },
        SupportedFormat {
            channels: 1,
            min_sample_rate: 8_000,
            max_sample_rate: 192_000,
            sample_type: SampleType::F32,
        },
        SupportedFormat {
            channels: 2,
            min_sample_rate: 8_000,
            max_sample_rate: 192_000,
            sample_type: SampleType::F32,
        },
    ]
}

/// Maps a `tinypipewire::Error` into an `edge_ear::error::Error`.
pub(crate) fn map_tinypipewire_error(device: Device, err: tinypipewire::Error) -> Error {
    match err {
        tinypipewire::Error::ConnectFailed => Error::Backend {
            device,
            reason: "failed to connect to PipeWire daemon".to_string(),
        },
        tinypipewire::Error::SourceUnavailable => Error::NoDevice(device),
        tinypipewire::Error::InvalidArgument => Error::Backend {
            device,
            reason: "invalid argument for PipeWire stream".to_string(),
        },
        tinypipewire::Error::InvalidFormat => Error::Backend {
            device,
            reason: "unsupported audio format for PipeWire stream".to_string(),
        },
        tinypipewire::Error::CreateFailed => Error::Backend {
            device,
            reason: "failed to create PipeWire stream".to_string(),
        },
        other => Error::Backend {
            device,
            reason: other.to_string(),
        },
    }
}

/// Converts raw PCM bytes into `Samples`.
pub(crate) fn bytes_to_samples(bytes: &[u8], sample_type: SampleType) -> Samples {
    match sample_type {
        SampleType::I16 => {
            let samples: Vec<i16> = bytes
                .chunks_exact(2)
                .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
                .collect();
            Samples::I16(samples)
        }
        SampleType::F32 => {
            let samples: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect();
            Samples::F32(samples)
        }
    }
}

/// Lists available devices for input or output.
fn list_devices(is_input: bool) -> Result<Vec<DeviceInfo>> {
    let device = device_of(is_input);
    let stream = if is_input {
        Stream::audio_capture(|_| {})
    } else {
        Stream::playback(|_| {})
    }
    .map_err(|e| map_tinypipewire_error(device, e))?;

    let targets = stream
        .targets()
        .map_err(|e| map_tinypipewire_error(device, e))?;

    let mut devices = Vec::with_capacity(targets.len());
    for (i, t) in targets.into_iter().enumerate() {
        let id = if !t.serial.is_empty() {
            t.serial
        } else {
            t.name.clone()
        };
        let name = if !t.description.is_empty() {
            t.description
        } else {
            t.name
        };
        devices.push(DeviceInfo {
            id,
            name,
            // TODO: PipeWire lists targets in no particular order, so this can
            // flag the wrong device. Use the session's default.audio.{source,sink}
            // once tinypipewire exposes it.
            is_default: i == 0,
        });
    }
    Ok(devices)
}

/// Hands queued audio to the PipeWire playback buffer.
///
/// What does not fit in one buffer cycle is kept for the next.
pub(crate) struct Drain {
    queue: Arc<Ring<Samples>>,
    held: Option<(Samples, usize)>,
    sample_type: SampleType,
}

impl Drain {
    pub(crate) fn new(queue: Arc<Ring<Samples>>, sample_type: SampleType) -> Self {
        Self {
            queue,
            held: None,
            sample_type,
        }
    }

    /// Fills the output byte slice, padding with silence when the queue runs dry.
    /// Returns how many samples were real audio.
    pub(crate) fn fill(&mut self, mut out: &mut [u8]) -> usize {
        let mut real_bytes = 0;
        while !out.is_empty() {
            if self.held.is_none() {
                self.held = self.queue.try_take().map(|taken| (taken.item, 0));
            }
            let Some((block, at)) = self.held.as_mut() else {
                break;
            };

            let written_bytes = match (self.sample_type, &*block) {
                (SampleType::I16, Samples::I16(s)) => {
                    let needed = out.len() / 2;
                    let avail = s.len() - *at;
                    let count = needed.min(avail);
                    for i in 0..count {
                        let b = s[*at + i].to_le_bytes();
                        out[i * 2..i * 2 + 2].copy_from_slice(&b);
                    }
                    *at += count;
                    count * 2
                }
                (SampleType::I16, Samples::F32(s)) => {
                    let needed = out.len() / 2;
                    let avail = s.len() - *at;
                    let count = needed.min(avail);
                    for i in 0..count {
                        let val = (s[*at + i] * 32767.0).clamp(-32768.0, 32767.0) as i16;
                        let b = val.to_le_bytes();
                        out[i * 2..i * 2 + 2].copy_from_slice(&b);
                    }
                    *at += count;
                    count * 2
                }
                (SampleType::F32, Samples::F32(s)) => {
                    let needed = out.len() / 4;
                    let avail = s.len() - *at;
                    let count = needed.min(avail);
                    for i in 0..count {
                        let b = s[*at + i].to_le_bytes();
                        out[i * 4..i * 4 + 4].copy_from_slice(&b);
                    }
                    *at += count;
                    count * 4
                }
                (SampleType::F32, Samples::I16(s)) => {
                    let needed = out.len() / 4;
                    let avail = s.len() - *at;
                    let count = needed.min(avail);
                    for i in 0..count {
                        let val = s[*at + i] as f32 / 32768.0;
                        let b = val.to_le_bytes();
                        out[i * 4..i * 4 + 4].copy_from_slice(&b);
                    }
                    *at += count;
                    count * 4
                }
            };

            if written_bytes == 0 {
                break;
            }

            if *at >= block.len() {
                self.held = None;
            }

            real_bytes += written_bytes;
            out = &mut out[written_bytes..];
        }

        // Fill any remaining unwritten space with silence.
        out.fill(0);
        real_bytes / sample_bytes(self.sample_type)
    }
}

fn sample_bytes(sample_type: SampleType) -> usize {
    match sample_type {
        SampleType::I16 => 2,
        SampleType::F32 => 4,
    }
}

/// How long from now until `pts`, a time on PipeWire's clock. `None` when
/// the clock cannot be read or the time has already passed.
fn until(pts: i64) -> Option<Duration> {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes only into the timespec it is handed.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
        return None;
    }
    let now = Duration::new(
        u64::try_from(now.tv_sec).ok()?,
        u32::try_from(now.tv_nsec).ok()?,
    );
    Duration::from_nanos(u64::try_from(pts).ok()?)
        .checked_sub(now)
        .filter(|ahead| !ahead.is_zero())
}

/// An open PipeWire input stream.
pub struct TinypipewireInput {
    queue: Arc<Ring<Samples>>,
    losses: LossReport,
    format: AudioFormat,
    stream: Option<Stream>,
}

impl InputStream for TinypipewireInput {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn read(&mut self) -> Result<Samples> {
        let taken = self.queue.take(None)?;
        self.losses.note(taken.dropped_before);
        Ok(taken.item)
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(stream) = self.stream.take() {
            if let Err(e) = stream.stop(false) {
                log::error!("the microphone stream would not stop: {e}");
            }
            drop(stream);
        }
        self.queue.close();
        Ok(())
    }
}

impl Drop for TinypipewireInput {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// An open PipeWire output stream.
pub struct TinypipewireOutput {
    queue: Arc<Ring<Samples>>,
    playout: Arc<Playout>,
    format: AudioFormat,
    stream: Option<Stream>,
}

impl OutputStream for TinypipewireOutput {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn write(&mut self, samples: &Samples) -> Result<()> {
        self.queue
            .push_before(samples.clone(), Duration::from_millis(100))
    }

    fn heard_at(&self, position: u64) -> Option<Instant> {
        self.playout.heard_at(position)
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(stream) = self.stream.take() {
            if let Err(e) = stream.stop(true) {
                log::error!("the speaker stream would not stop: {e}");
            }
            drop(stream);
        }
        self.queue.close();
        Ok(())
    }
}

impl Drop for TinypipewireOutput {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl AudioBackend for TinypipewireBackend {
    fn open_input(&mut self, req: &FormatRequest) -> Result<Box<dyn InputStream>> {
        let target = self.pick_device(true, req.device.as_deref())?;
        let format = req.wanted.unwrap_or(AudioFormat::mono_16k());

        crate::refuse_unless_offered(&supported_formats(), format, Device::Input)?;

        let sample_format = match format.sample_type {
            SampleType::I16 => SampleFormat::S16,
            SampleType::F32 => SampleFormat::F32,
        };
        let audio_cfg =
            AudioConfig::new(format.sample_rate, format.channels as u32).with_format(sample_format);

        let queue = Arc::new(Ring::new(DEVICE_QUEUE_BLOCKS));
        let feed = Arc::clone(&queue);
        let sample_type = format.sample_type;

        let stream = Stream::audio_capture(move |buf| {
            if let Some(bytes) = buf.data()
                && !bytes.is_empty()
            {
                feed.push(bytes_to_samples(bytes, sample_type));
            }
        })
        .map_err(|e| map_tinypipewire_error(Device::Input, e))?;

        // This runs when the source is lost rather than per block, so
        // logging is safe. Losing the report is not worth failing over.
        if let Err(e) =
            stream.set_error_callback(|err| log::error!("microphone stream error: {err}"))
        {
            log::warn!("the microphone stream will not report faults: {e}");
        }

        if let Some(ref target) = target {
            stream
                .set_routing(Routing::Autoconnect(Some(target)))
                .map_err(|e| map_tinypipewire_error(Device::Input, e))?;
        } else {
            stream
                .set_routing(Routing::Autoconnect(None))
                .map_err(|e| map_tinypipewire_error(Device::Input, e))?;
        }

        stream
            .set_audio_config(&audio_cfg)
            .map_err(|e| map_tinypipewire_error(Device::Input, e))?;

        stream
            .start()
            .map_err(|e| map_tinypipewire_error(Device::Input, e))?;

        Ok(Box::new(TinypipewireInput {
            queue,
            losses: LossReport::new("capture from the microphone"),
            format,
            stream: Some(stream),
        }))
    }

    fn open_output(&mut self, req: &FormatRequest) -> Result<Box<dyn OutputStream>> {
        let target = self.pick_device(false, req.device.as_deref())?;
        let format = req
            .wanted
            .unwrap_or(AudioFormat::new(48_000, 2, SampleType::I16));

        crate::refuse_unless_offered(&supported_formats(), format, Device::Output)?;

        let sample_format = match format.sample_type {
            SampleType::I16 => SampleFormat::S16,
            SampleType::F32 => SampleFormat::F32,
        };
        let audio_cfg =
            AudioConfig::new(format.sample_rate, format.channels as u32).with_format(sample_format);

        let queue = Arc::new(Ring::new(DEVICE_QUEUE_BLOCKS));
        let playout = Arc::new(Playout::new(format));
        let mut drain = Drain::new(Arc::clone(&queue), format.sample_type);

        let heard = Arc::clone(&playout);
        let stream = Stream::playback(move |buf| {
            let avail = buf.available();
            let real = drain.fill(buf.as_mut_slice());
            buf.set_filled(avail);
            let ahead = buf
                .pts()
                .and_then(until)
                .unwrap_or_else(|| guessed_ahead(avail / sample_bytes(format.sample_type), format));
            heard.took(real, ahead);
        })
        .map_err(|e| map_tinypipewire_error(Device::Output, e))?;

        // This runs when the sink is lost rather than per block, so
        // logging is safe. Losing the report is not worth failing over.
        if let Err(e) = stream.set_error_callback(|err| log::error!("speaker stream error: {err}"))
        {
            log::warn!("the speaker stream will not report faults: {e}");
        }

        if let Some(ref target) = target {
            stream
                .set_routing(Routing::Autoconnect(Some(target)))
                .map_err(|e| map_tinypipewire_error(Device::Output, e))?;
        } else {
            stream
                .set_routing(Routing::Autoconnect(None))
                .map_err(|e| map_tinypipewire_error(Device::Output, e))?;
        }

        stream
            .set_audio_config(&audio_cfg)
            .map_err(|e| map_tinypipewire_error(Device::Output, e))?;

        stream
            .start()
            .map_err(|e| map_tinypipewire_error(Device::Output, e))?;

        Ok(Box::new(TinypipewireOutput {
            queue,
            playout,
            format,
            stream: Some(stream),
        }))
    }

    fn input_formats(&self, device: Option<&str>) -> Result<Vec<SupportedFormat>> {
        self.pick_device(true, device)?;
        Ok(supported_formats())
    }

    fn output_formats(&self, device: Option<&str>) -> Result<Vec<SupportedFormat>> {
        self.pick_device(false, device)?;
        Ok(supported_formats())
    }

    fn input_devices(&self) -> Result<Vec<DeviceInfo>> {
        list_devices(true)
    }

    fn output_devices(&self) -> Result<Vec<DeviceInfo>> {
        list_devices(false)
    }

    fn describe(&self) -> &'static str {
        "tinypipewire"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(block: Samples, sample_type: SampleType) -> Drain {
        let queue = Arc::new(Ring::new(4));
        queue.push(block);
        Drain::new(queue, sample_type)
    }

    #[test]
    fn what_did_not_fit_is_handed_over_next_time() {
        let mut drain = queued(
            Samples::I16((0..512).map(|n| n as i16).collect()),
            SampleType::I16,
        );

        let mut first = [0u8; 512]; // 256 samples of i16
        drain.fill(&mut first);
        let mut second = [0u8; 512]; // remaining 256 samples of i16
        drain.fill(&mut second);

        let first_samples: Vec<i16> = first
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        let second_samples: Vec<i16> = second
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();

        assert_eq!(
            first_samples[0], 0,
            "the first buffer starts at the beginning"
        );
        assert_eq!(first_samples[255], 255, "end of first buffer");
        assert_eq!(second_samples[0], 256, "the rest of the block follows it");
        assert_eq!(second_samples[255], 511, "right to the end of it");
    }

    fn as_i16(bytes: &[u8]) -> Vec<i16> {
        bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    fn as_f32(bytes: &[u8]) -> Vec<f32> {
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    #[test]
    fn a_speaker_of_any_sample_type_is_handed_every_sample() {
        let mut drain = queued(Samples::I16(vec![16_000; 8]), SampleType::I16);
        let mut room = [0u8; 16];
        drain.fill(&mut room);
        assert_eq!(as_i16(&room), vec![16_000; 8], "I16 -> I16");

        let mut drain = queued(Samples::F32(vec![0.5; 8]), SampleType::I16);
        let mut room = [0u8; 16];
        drain.fill(&mut room);
        assert_eq!(as_i16(&room), vec![16_383; 8], "F32 -> I16");

        let mut drain = queued(Samples::F32(vec![0.5; 8]), SampleType::F32);
        let mut room = [0u8; 32];
        drain.fill(&mut room);
        assert_eq!(as_f32(&room), vec![0.5; 8], "F32 -> F32");

        let mut drain = queued(Samples::I16(vec![16_384; 8]), SampleType::F32);
        let mut room = [0u8; 32];
        drain.fill(&mut room);
        assert_eq!(as_f32(&room), vec![0.5; 8], "I16 -> F32");
    }

    #[test]
    fn a_short_f32_block_is_followed_by_silence_not_cut_in_half() {
        let played: Vec<f32> = (1..=4).map(|n| n as f32 / 8.0).collect();
        for source in [
            Samples::F32(played.clone()),
            Samples::I16(played.iter().map(|s| (s * 32768.0) as i16).collect()),
        ] {
            let mut drain = queued(source, SampleType::F32);
            let mut room = [0xffu8; 32];
            drain.fill(&mut room);

            let mut expected = played.clone();
            expected.resize(8, 0.0);
            assert_eq!(as_f32(&room), expected);
        }
    }

    #[test]
    fn a_long_f32_block_carries_over_without_losing_samples() {
        let played: Vec<f32> = (0..12).map(|n| n as f32 / 16.0).collect();
        for source in [
            Samples::F32(played.clone()),
            Samples::I16(played.iter().map(|s| (s * 32768.0) as i16).collect()),
        ] {
            let mut drain = queued(source, SampleType::F32);
            let mut first = [0u8; 32];
            drain.fill(&mut first);
            let mut second = [0u8; 16];
            drain.fill(&mut second);

            assert_eq!(as_f32(&first), played[..8]);
            assert_eq!(as_f32(&second), played[8..]);
        }
    }

    #[test]
    fn a_dry_queue_is_silence_and_not_the_last_thing_played() {
        let mut drain = Drain::new(Arc::new(Ring::new(4)), SampleType::I16);
        let mut out = [123u8; 16];
        drain.fill(&mut out);
        assert!(out.iter().all(|s| *s == 0), "expected silence on dry queue");
    }

    #[test]
    fn bytes_to_samples_round_trip() {
        let original_i16: Vec<i16> = vec![0, 100, -200, 32767, -32768];
        let bytes_i16: Vec<u8> = original_i16.iter().flat_map(|s| s.to_le_bytes()).collect();
        let samples_i16 = bytes_to_samples(&bytes_i16, SampleType::I16);
        assert_eq!(samples_i16, Samples::I16(original_i16));

        let original_f32: Vec<f32> = vec![0.0, 0.5, -0.5, 1.0, -1.0];
        let bytes_f32: Vec<u8> = original_f32.iter().flat_map(|s| s.to_le_bytes()).collect();
        let samples_f32 = bytes_to_samples(&bytes_f32, SampleType::F32);
        assert_eq!(samples_f32, Samples::F32(original_f32));
    }

    #[test]
    fn error_mapping_translates_correctly() {
        let err = map_tinypipewire_error(Device::Input, tinypipewire::Error::ConnectFailed);
        assert!(matches!(
            err,
            Error::Backend {
                device: Device::Input,
                ..
            }
        ));

        let err = map_tinypipewire_error(Device::Output, tinypipewire::Error::SourceUnavailable);
        assert!(matches!(err, Error::NoDevice(Device::Output)));

        let err = map_tinypipewire_error(Device::Input, tinypipewire::Error::InvalidArgument);
        assert!(matches!(
            err,
            Error::Backend {
                device: Device::Input,
                ..
            }
        ));
    }

    #[test]
    fn supported_formats_covers_standard_settings() {
        let formats = supported_formats();
        assert!(formats.iter().any(|f| f.covers(&AudioFormat::mono_16k())));
        assert!(
            formats
                .iter()
                .any(|f| f.covers(&AudioFormat::new(48_000, 2, SampleType::I16)))
        );
        assert!(
            formats
                .iter()
                .any(|f| f.covers(&AudioFormat::new(44_100, 2, SampleType::F32)))
        );
        assert!(
            formats
                .iter()
                .any(|f| f.covers(&AudioFormat::new(96_000, 1, SampleType::F32)))
        );

        // 3 channels is not covered
        assert!(
            !formats
                .iter()
                .any(|f| f.covers(&AudioFormat::new(16_000, 3, SampleType::I16)))
        );
        // 4000 Hz is below 8000 Hz minimum
        assert!(
            !formats
                .iter()
                .any(|f| f.covers(&AudioFormat::new(4_000, 1, SampleType::I16)))
        );
    }

    #[test]
    fn backend_description_is_tinypipewire() {
        let backend = TinypipewireBackend::new().unwrap();
        assert_eq!(backend.describe(), "tinypipewire");
    }
}
