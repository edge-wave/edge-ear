//! The PipeWire audio backend using tinypipewire-rs.
//!
//! Audio capture and playback are bridged between PipeWire's real-time thread loop
//! and edge-ear's ring queues so neither waits on the other.

use std::sync::Arc;
use std::time::Duration;

use tinypipewire::{AudioConfig, Routing, SampleFormat, Stream};

use super::{AudioBackend, DeviceInfo, FormatRequest, InputStream, OutputStream, SupportedFormat};
use crate::backend::device_of;
use crate::capture::Samples;
use crate::capture::ring::Ring;
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
    pub(crate) fn fill(&mut self, mut out: &mut [u8]) {
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

            out = &mut out[written_bytes..];
        }

        // Fill any remaining unwritten space with silence.
        out.fill(0);
    }
}

/// An open PipeWire input stream.
pub struct TinypipewireInput {
    queue: Arc<Ring<Samples>>,
    format: AudioFormat,
    stream: Option<Stream>,
}

impl InputStream for TinypipewireInput {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn read(&mut self) -> Result<Samples> {
        Ok(self.queue.take(None)?.item)
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(stream) = self.stream.take() {
            let _ = stream.stop(false);
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

    fn stop(&mut self) -> Result<()> {
        if let Some(stream) = self.stream.take() {
            let _ = stream.stop(true);
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
        let mut drain = Drain::new(Arc::clone(&queue), format.sample_type);

        let stream = Stream::playback(move |buf| {
            let avail = buf.available();
            drain.fill(buf.as_mut_slice());
            buf.set_filled(avail);
        })
        .map_err(|e| map_tinypipewire_error(Device::Output, e))?;

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
