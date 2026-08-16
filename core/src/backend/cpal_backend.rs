//! The default backend: real devices through cpal.
//!
//! cpal pushes audio from its own thread; the rest of this library
//! pulls. The bridge is a bounded queue, so the device callback hands
//! its block over and returns at once. It never waits on us, and we
//! never wait on it.
//!
//! The cpal stream is built, played, and dropped on one thread of its
//! own and never crosses a thread boundary. That sidesteps the
//! question of whether a given platform's stream can be moved.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, StreamConfig, SupportedStreamConfig};

use super::{AudioBackend, DeviceInfo, FormatRequest, InputStream, OutputStream};
use crate::capture::Samples;
use crate::capture::ring::Ring;
use crate::config::{AudioFormat, Device, SampleType};
use crate::error::{Error, Result};

/// Blocks the device queue holds before the oldest goes. The capture
/// thread drains this immediately, so it only fills if something above
/// has stalled, and dropping is better than growing.
const DEVICE_QUEUE_BLOCKS: usize = 32;

pub struct CpalBackend {
    host: cpal::Host,
}

impl CpalBackend {
    pub fn new() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }

    fn pick_input(&self, name: Option<&str>) -> Result<cpal::Device> {
        match name {
            None => self
                .host
                .default_input_device()
                .ok_or(Error::NoDevice(Device::Input)),
            Some(wanted) => self
                .host
                .input_devices()
                .map_err(|e| devices_error(Device::Input, &e.to_string()))?
                .find(|d| matches_device(d, wanted))
                .ok_or(Error::NoDevice(Device::Input)),
        }
    }

    fn pick_output(&self, name: Option<&str>) -> Result<cpal::Device> {
        match name {
            None => self
                .host
                .default_output_device()
                .ok_or(Error::NoDevice(Device::Output)),
            Some(wanted) => self
                .host
                .output_devices()
                .map_err(|e| devices_error(Device::Output, &e.to_string()))?
                .find(|d| matches_device(d, wanted))
                .ok_or(Error::NoDevice(Device::Output)),
        }
    }
}

impl Default for CpalBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Turn a device failure into one of the three outcomes an application
/// must be able to tell apart.
///
/// A refused microphone is guessed from the message, which is the only
/// signal cpal gives. Asking the system directly is platform work: on
/// macOS that means the authorisation status, which belongs in a later
/// platform pass.
fn classify(device: Device, message: &str) -> Error {
    let lowered = message.to_ascii_lowercase();
    if lowered.contains("permission")
        || lowered.contains("denied")
        || lowered.contains("not authorized")
        || lowered.contains("not authorised")
        || lowered.contains("unauthorized")
    {
        return Error::PermissionDenied;
    }
    if lowered.contains("no such device")
        || lowered.contains("device not available")
        || lowered.contains("nodevice")
    {
        return Error::NoDevice(device);
    }
    Error::Backend {
        device,
        reason: message.to_string(),
    }
}

fn devices_error(device: Device, message: &str) -> Error {
    classify(device, message)
}

fn to_audio_format(config: &SupportedStreamConfig) -> Result<AudioFormat> {
    let sample_type = match config.sample_format() {
        SampleFormat::I16 => SampleType::I16,
        SampleFormat::F32 => SampleType::F32,
        // Anything else is converted on the way in, and reported as the
        // nearest thing this library speaks.
        _ => SampleType::F32,
    };
    Ok(AudioFormat::new(
        config.sample_rate(),
        config.channels(),
        sample_type,
    ))
}

/// Told to the owner thread; carries the outcome of building a stream.
type Started = std::result::Result<(), String>;

struct Gate {
    stop: AtomicBool,
    waker: Condvar,
    parked: Mutex<()>,
}

impl Gate {
    fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            waker: Condvar::new(),
            parked: Mutex::new(()),
        }
    }

    fn wait_until_stopped(&self) {
        let mut guard = self.parked.lock().unwrap_or_else(|e| e.into_inner());
        while !self.stop.load(Ordering::Relaxed) {
            guard = self
                .waker
                .wait(guard)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn signal_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.waker.notify_all();
    }
}

pub struct CpalInput {
    queue: Arc<Ring<Samples>>,
    format: AudioFormat,
    gate: Arc<Gate>,
    owner: Option<JoinHandle<()>>,
}

impl InputStream for CpalInput {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn read(&mut self) -> Result<Samples> {
        Ok(self.queue.take(None)?.item)
    }

    fn stop(&mut self) -> Result<()> {
        self.gate.signal_stop();
        self.queue.close();
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
        Ok(())
    }
}

impl Drop for CpalInput {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub struct CpalOutput {
    queue: Arc<Ring<Samples>>,
    format: AudioFormat,
    gate: Arc<Gate>,
    owner: Option<JoinHandle<()>>,
}

impl OutputStream for CpalOutput {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn write(&mut self, samples: &Samples) -> Result<()> {
        self.queue.push(samples.clone());
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.gate.signal_stop();
        self.queue.close();
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
        Ok(())
    }
}

impl Drop for CpalOutput {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Copy whatever the device gave us into our own shape.
fn data_to_samples(data: &cpal::Data, format: SampleFormat) -> Samples {
    match format {
        SampleFormat::I16 => Samples::I16(data.as_slice::<i16>().unwrap_or(&[]).to_vec()),
        SampleFormat::F32 => Samples::F32(data.as_slice::<f32>().unwrap_or(&[]).to_vec()),
        SampleFormat::U8 => Samples::F32(
            data.as_slice::<u8>()
                .unwrap_or(&[])
                .iter()
                .map(|s| s.to_sample::<f32>())
                .collect(),
        ),
        SampleFormat::I32 => Samples::F32(
            data.as_slice::<i32>()
                .unwrap_or(&[])
                .iter()
                .map(|s| s.to_sample::<f32>())
                .collect(),
        ),
        SampleFormat::F64 => Samples::F32(
            data.as_slice::<f64>()
                .unwrap_or(&[])
                .iter()
                .map(|s| *s as f32)
                .collect(),
        ),
        _ => Samples::F32(Vec::new()),
    }
}

fn fill_from_samples(data: &mut cpal::Data, format: SampleFormat, queue: &Ring<Samples>) {
    let wanted = data.len();
    match format {
        SampleFormat::I16 => {
            let out = data.as_slice_mut::<i16>().unwrap_or(&mut []);
            out.fill(0);
            fill_i16(out, wanted, queue);
        }
        SampleFormat::F32 => {
            let out = data.as_slice_mut::<f32>().unwrap_or(&mut []);
            out.fill(0.0);
            fill_f32(out, wanted, queue);
        }
        _ => {}
    }
}

fn fill_i16(out: &mut [i16], wanted: usize, queue: &Ring<Samples>) {
    let mut written = 0;
    while written < wanted {
        let Some(taken) = queue.try_take() else { break };
        match taken.item {
            Samples::I16(v) => {
                let n = v.len().min(wanted - written);
                out[written..written + n].copy_from_slice(&v[..n]);
                written += n;
            }
            Samples::F32(v) => {
                for s in v.iter().take(wanted - written) {
                    out[written] = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                    written += 1;
                }
            }
        }
    }
}

fn fill_f32(out: &mut [f32], wanted: usize, queue: &Ring<Samples>) {
    let mut written = 0;
    while written < wanted {
        let Some(taken) = queue.try_take() else { break };
        match taken.item {
            Samples::F32(v) => {
                let n = v.len().min(wanted - written);
                out[written..written + n].copy_from_slice(&v[..n]);
                written += n;
            }
            Samples::I16(v) => {
                for s in v.iter().take(wanted - written) {
                    out[written] = *s as f32 / 32768.0;
                    written += 1;
                }
            }
        }
    }
}

fn wait_for_start(started: Receiver<Started>, device: Device) -> Result<()> {
    match started.recv() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(message)) => Err(classify(device, &message)),
        Err(_) => Err(Error::Backend {
            device,
            reason: "the device thread stopped before the stream opened".to_string(),
        }),
    }
}

impl AudioBackend for CpalBackend {
    fn open_input(&mut self, req: &FormatRequest) -> Result<Box<dyn InputStream>> {
        let device = self.pick_input(req.device.as_deref())?;
        let supported = device
            .default_input_config()
            .map_err(|e| classify(Device::Input, &e.to_string()))?;
        let format = to_audio_format(&supported)?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.into();

        let queue = Arc::new(Ring::new(DEVICE_QUEUE_BLOCKS));
        let gate = Arc::new(Gate::new());
        let (tx, rx): (SyncSender<Started>, Receiver<Started>) = sync_channel(1);

        let owner = {
            let queue = Arc::clone(&queue);
            let gate = Arc::clone(&gate);
            thread::Builder::new()
                .name("edge-ear-cpal-in".to_string())
                .spawn(move || {
                    let feed = Arc::clone(&queue);
                    let built = device.build_input_stream_raw(
                        config,
                        sample_format,
                        move |data, _| {
                            // Runs on the device's own thread. Hand the
                            // block over and return; never wait here.
                            feed.push(data_to_samples(data, sample_format));
                        },
                        |_err| {},
                        None,
                    );

                    let stream = match built {
                        Ok(stream) => stream,
                        Err(e) => {
                            let _ = tx.send(Err(e.to_string()));
                            return;
                        }
                    };
                    if let Err(e) = stream.play() {
                        let _ = tx.send(Err(e.to_string()));
                        return;
                    }
                    let _ = tx.send(Ok(()));

                    gate.wait_until_stopped();
                    drop(stream);
                    queue.close();
                })
                .map_err(|e| Error::Backend {
                    device: Device::Input,
                    reason: format!("device thread would not start: {e}"),
                })?
        };

        if let Err(e) = wait_for_start(rx, Device::Input) {
            gate.signal_stop();
            let _ = owner.join();
            return Err(e);
        }

        Ok(Box::new(CpalInput {
            queue,
            format,
            gate,
            owner: Some(owner),
        }))
    }

    fn open_output(&mut self, req: &FormatRequest) -> Result<Box<dyn OutputStream>> {
        let device = self.pick_output(req.device.as_deref())?;
        let supported = device
            .default_output_config()
            .map_err(|e| classify(Device::Output, &e.to_string()))?;
        let format = to_audio_format(&supported)?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.into();

        let queue = Arc::new(Ring::new(DEVICE_QUEUE_BLOCKS));
        let gate = Arc::new(Gate::new());
        let (tx, rx): (SyncSender<Started>, Receiver<Started>) = sync_channel(1);

        let owner = {
            let queue = Arc::clone(&queue);
            let gate = Arc::clone(&gate);
            thread::Builder::new()
                .name("edge-ear-cpal-out".to_string())
                .spawn(move || {
                    let drain = Arc::clone(&queue);
                    let built = device.build_output_stream_raw(
                        config,
                        sample_format,
                        move |data, _| {
                            // Silence when there is nothing queued, so a
                            // gap sounds like a gap rather than a click.
                            fill_from_samples(data, sample_format, &drain);
                        },
                        |_err| {},
                        None,
                    );

                    let stream = match built {
                        Ok(stream) => stream,
                        Err(e) => {
                            let _ = tx.send(Err(e.to_string()));
                            return;
                        }
                    };
                    if let Err(e) = stream.play() {
                        let _ = tx.send(Err(e.to_string()));
                        return;
                    }
                    let _ = tx.send(Ok(()));

                    gate.wait_until_stopped();
                    drop(stream);
                    queue.close();
                })
                .map_err(|e| Error::Backend {
                    device: Device::Output,
                    reason: format!("device thread would not start: {e}"),
                })?
        };

        if let Err(e) = wait_for_start(rx, Device::Output) {
            gate.signal_stop();
            let _ = owner.join();
            return Err(e);
        }

        Ok(Box::new(CpalOutput {
            queue,
            format,
            gate,
            owner: Some(owner),
        }))
    }

    fn input_devices(&self) -> Result<Vec<DeviceInfo>> {
        let default = self.host.default_input_device().and_then(device_id);
        let devices = self
            .host
            .input_devices()
            .map_err(|e| devices_error(Device::Input, &e.to_string()))?;
        Ok(list(devices, default))
    }

    fn output_devices(&self) -> Result<Vec<DeviceInfo>> {
        let default = self.host.default_output_device().and_then(device_id);
        let devices = self
            .host
            .output_devices()
            .map_err(|e| devices_error(Device::Output, &e.to_string()))?;
        Ok(list(devices, default))
    }

    fn describe(&self) -> &'static str {
        "cpal"
    }
}

/// The identifier an application stores to pick this device again.
/// Devices without one cannot be named reliably, so they are skipped.
fn device_id(device: cpal::Device) -> Option<String> {
    device.id().ok().map(|id| id.to_string())
}

/// Match on the stored identifier first. A name is accepted too, for
/// convenience, but it can match more than one device so the
/// identifier always wins.
fn matches_device(device: &cpal::Device, wanted: &str) -> bool {
    if let Ok(id) = device.id()
        && id.to_string() == wanted
    {
        return true;
    }
    device.to_string() == wanted
}

fn list(devices: impl Iterator<Item = cpal::Device>, default: Option<String>) -> Vec<DeviceInfo> {
    devices
        .filter_map(|d| {
            let id = d.id().ok()?.to_string();
            Some(DeviceInfo {
                is_default: Some(&id) == default.as_ref(),
                id,
                name: d.to_string(),
            })
        })
        .collect()
}

/// Kept so a caller can tell how long a device block covers.
pub fn block_duration(format: AudioFormat, samples: usize) -> Duration {
    let frames = samples / format.channels.max(1) as usize;
    Duration::from_secs_f64(frames as f64 / format.sample_rate as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_wording_becomes_its_own_error() {
        for message in [
            "Permission denied",
            "device not authorized",
            "ALSA: permission denied opening default",
        ] {
            let err = classify(Device::Input, message);
            assert!(
                matches!(err, Error::PermissionDenied),
                "{message} gave {err}"
            );
        }
    }

    #[test]
    fn a_missing_device_stays_separate_from_a_refused_one() {
        let err = classify(Device::Input, "No such device");
        assert!(matches!(err, Error::NoDevice(Device::Input)), "{err}");
    }

    #[test]
    fn anything_else_keeps_its_own_wording() {
        let err = classify(Device::Output, "sample rate not supported");
        assert!(matches!(err, Error::Backend { .. }), "{err}");
        assert!(err.to_string().contains("sample rate"), "{err}");
    }

    /// Needs a real machine with audio devices, so it is not part of
    /// the normal run.
    #[test]
    #[ignore]
    fn real_devices_can_be_listed() {
        let backend = CpalBackend::new();
        let inputs = backend.input_devices().expect("input devices");
        println!("inputs: {inputs:?}");
        assert!(!inputs.is_empty(), "expected at least one input device");
    }

    #[test]
    #[ignore]
    fn a_real_microphone_produces_audio() {
        let mut backend = CpalBackend::new();
        let mut input = backend
            .open_input(&FormatRequest {
                device: None,
                preferred: AudioFormat::mono_16k(),
            })
            .expect("open the default microphone");

        println!("device format: {:?}", input.format());
        let block = input.read().expect("audio");
        assert!(!block.is_empty(), "the device produced an empty block");
        input.stop().unwrap();
    }
}
