//! The default backend: real devices through cpal. It pushes, we pull,
//! and a bounded queue bridges the two so neither waits on the other.
//! Its stream lives on one thread and never crosses to another.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, StreamConfig, SupportedStreamConfig};

use super::playout::{Playout, fade_out, guessed_ahead};
use super::{AudioBackend, DeviceInfo, FormatRequest, InputStream, OutputStream, SupportedFormat};
use crate::capture::Samples;
use crate::capture::ring::{LossReport, Ring};
use crate::config::{AudioFormat, Device, SampleType};
use crate::echo::EchoReference;
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

/// Sort a device failure into the outcomes an application must tell
/// apart, guessed from the message. On macOS a refusal may instead open
/// and deliver silence, never reaching here and looking like quiet.
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

/// The device's own config for a format it said it would take. Refused
/// by name of what it does offer, rather than quietly given something
/// else.
fn pick_config(
    offered: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    wanted: AudioFormat,
    device: Device,
) -> Result<SupportedStreamConfig> {
    let mut seen = Vec::new();
    for range in offered {
        let found = SupportedFormat {
            channels: range.channels(),
            min_sample_rate: range.min_sample_rate(),
            max_sample_rate: range.max_sample_rate(),
            sample_type: sample_type_of(range.sample_format()),
        };
        if found.covers(&wanted) {
            return Ok(range.with_sample_rate(wanted.sample_rate));
        }
        seen.push(found);
    }
    Err(Error::DeviceFormat {
        device,
        got: format!(
            "{} Hz, {} channels, {}",
            wanted.sample_rate, wanted.channels, wanted.sample_type
        ),
        offered: crate::describe_offered(&seen),
    })
}

/// Gather what a device offers, in the terms this library speaks. Two
/// device formats can land on one entry, so the same one is kept once.
fn collect_formats(
    ranges: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
) -> Vec<SupportedFormat> {
    let mut out: Vec<SupportedFormat> = Vec::new();
    for range in ranges {
        let found = SupportedFormat {
            channels: range.channels(),
            min_sample_rate: range.min_sample_rate(),
            max_sample_rate: range.max_sample_rate(),
            sample_type: sample_type_of(range.sample_format()),
        };
        if !out.contains(&found) {
            out.push(found);
        }
    }
    out
}

/// What this library hands over for a device's own spelling. Anything
/// it does not name itself arrives converted, as floats.
fn sample_type_of(format: SampleFormat) -> SampleType {
    match format {
        SampleFormat::I16 => SampleType::I16,
        _ => SampleType::F32,
    }
}

fn to_audio_format(config: &SupportedStreamConfig) -> Result<AudioFormat> {
    let sample_type = sample_type_of(config.sample_format());
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
    losses: LossReport,
    format: AudioFormat,
    gate: Arc<Gate>,
    owner: Option<JoinHandle<()>>,
}

impl InputStream for CpalInput {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn read(&mut self) -> Result<Samples> {
        let taken = self.queue.take(None)?;
        self.losses.note(taken.dropped_before);
        Ok(taken.item)
    }

    fn stop(&mut self) -> Result<()> {
        self.gate.signal_stop();
        self.queue.close();
        if let Some(owner) = self.owner.take() {
            crate::join_worker(owner, "microphone device");
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
    playout: Arc<Playout>,
    format: AudioFormat,
    gate: Arc<Gate>,
    owner: Option<JoinHandle<()>>,
}

impl OutputStream for CpalOutput {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn write(&mut self, samples: &Samples) -> Result<()> {
        // Wait for room rather than drop: unplayed audio going missing
        // is audible. Bounded, so a stopping player is never wedged.
        self.queue
            .push_before(samples.clone(), Duration::from_millis(100))
    }

    fn heard_at(&self, position: u64) -> Option<Instant> {
        self.playout.heard_at(position)
    }

    fn flush(&mut self) -> u64 {
        let format = self.format;
        self.queue.edit(|queued| fade_out(queued, format))
    }

    fn tap(&mut self, reference: Arc<EchoReference>) -> bool {
        self.playout.set_tap(reference);
        true
    }

    fn stop(&mut self) -> Result<()> {
        self.gate.signal_stop();
        self.queue.close();
        if let Some(owner) = self.owner.take() {
            crate::join_worker(owner, "speaker device");
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

/// Hands queued audio to the device. What does not fit in one buffer
/// is kept for the next rather than going with the block it came in.
struct Drain {
    queue: Arc<Ring<Samples>>,
    /// The block being handed over, and how far into it we are.
    held: Option<(Samples, usize)>,
}

impl Drain {
    fn new(queue: Arc<Ring<Samples>>) -> Self {
        Self { queue, held: None }
    }

    /// Fill one device buffer, with silence wherever the queue ran dry.
    /// Returns how many samples were real audio.
    fn fill<T>(&mut self, out: &mut [T]) -> usize
    where
        T: Sample + FromSample<i16> + FromSample<f32>,
    {
        let mut written = 0;
        while written < out.len() {
            if self.held.is_none() {
                self.held = self.queue.try_take().map(|taken| (taken.item, 0));
            }
            let Some((block, at)) = self.held.as_mut() else {
                break;
            };
            let took = match block {
                Samples::I16(v) => convert(&v[*at..], &mut out[written..]),
                Samples::F32(v) => convert(&v[*at..], &mut out[written..]),
            };
            *at += took;
            written += took;
            if *at >= block.len() {
                self.held = None;
            }
        }
        out[written..].fill(T::EQUILIBRIUM);
        written
    }
}

/// As many samples as both sides have room for, in the device's own
/// spelling. Returns how many crossed.
fn convert<S, T>(from: &[S], to: &mut [T]) -> usize
where
    S: Sample,
    T: Sample + FromSample<S>,
{
    let crossing = from.len().min(to.len());
    for (slot, sample) in to.iter_mut().zip(from) {
        *slot = sample.to_sample::<T>();
    }
    crossing
}

/// A device takes whichever spelling it was opened with, and this
/// library speaks all of the ones cpal names.
fn fill_from_samples(data: &mut cpal::Data, format: SampleFormat, drain: &mut Drain) -> usize {
    fn hand<T: SizedSample + FromSample<i16> + FromSample<f32>>(
        data: &mut cpal::Data,
        drain: &mut Drain,
    ) -> usize {
        drain.fill(data.as_slice_mut::<T>().unwrap_or(&mut []))
    }

    match format {
        SampleFormat::I8 => hand::<i8>(data, drain),
        SampleFormat::I16 => hand::<i16>(data, drain),
        SampleFormat::I24 => hand::<cpal::I24>(data, drain),
        SampleFormat::I32 => hand::<i32>(data, drain),
        SampleFormat::I64 => hand::<i64>(data, drain),
        SampleFormat::U8 => hand::<u8>(data, drain),
        SampleFormat::U16 => hand::<u16>(data, drain),
        SampleFormat::U24 => hand::<cpal::U24>(data, drain),
        SampleFormat::U32 => hand::<u32>(data, drain),
        SampleFormat::U64 => hand::<u64>(data, drain),
        SampleFormat::F32 => hand::<f32>(data, drain),
        SampleFormat::F64 => hand::<f64>(data, drain),
        _ => 0,
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
        let supported = match req.wanted {
            Some(wanted) => {
                let offered = device
                    .supported_input_configs()
                    .map_err(|e| classify(Device::Input, &e.to_string()))?;
                pick_config(offered, wanted, Device::Input)?
            }
            None => device
                .default_input_config()
                .map_err(|e| classify(Device::Input, &e.to_string()))?,
        };
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
                        // This runs on a device fault rather than per block, so logging is safe.
                        |err| log::error!("microphone stream error: {err}"),
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
            crate::join_worker(owner, "microphone device");
            return Err(e);
        }

        Ok(Box::new(CpalInput {
            queue,
            losses: LossReport::new("capture from the microphone"),
            format,
            gate,
            owner: Some(owner),
        }))
    }

    fn open_output(&mut self, req: &FormatRequest) -> Result<Box<dyn OutputStream>> {
        let device = self.pick_output(req.device.as_deref())?;
        let supported = match req.wanted {
            Some(wanted) => {
                let offered = device
                    .supported_output_configs()
                    .map_err(|e| classify(Device::Output, &e.to_string()))?;
                pick_config(offered, wanted, Device::Output)?
            }
            None => device
                .default_output_config()
                .map_err(|e| classify(Device::Output, &e.to_string()))?,
        };
        let format = to_audio_format(&supported)?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.into();

        let queue = Arc::new(Ring::new(DEVICE_QUEUE_BLOCKS));
        let playout = Arc::new(Playout::new(format));
        let gate = Arc::new(Gate::new());
        let (tx, rx): (SyncSender<Started>, Receiver<Started>) = sync_channel(1);

        let owner = {
            let queue = Arc::clone(&queue);
            let playout = Arc::clone(&playout);
            let gate = Arc::clone(&gate);
            thread::Builder::new()
                .name("edge-ear-cpal-out".to_string())
                .spawn(move || {
                    let mut drain = Drain::new(Arc::clone(&queue));
                    let built = device.build_output_stream_raw(
                        config,
                        sample_format,
                        move |data, info: &cpal::OutputCallbackInfo| {
                            // Silence when there is nothing queued, so a
                            // gap sounds like a gap rather than a click.
                            let real = fill_from_samples(data, sample_format, &mut drain);
                            let stamp = info.timestamp();
                            let mut ahead = stamp.playback.duration_since(stamp.callback);
                            // A host that cannot tell reports no gap; one buffer is the least it is.
                            if ahead.is_zero() {
                                ahead = guessed_ahead(data.len(), format);
                            }
                            playout.took(real, ahead);
                            playout.played(ahead, || data_to_samples(data, sample_format));
                        },
                        // This runs on a device fault rather than per block, so logging is safe.
                        |err| log::error!("speaker stream error: {err}"),
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
            crate::join_worker(owner, "speaker device");
            return Err(e);
        }

        Ok(Box::new(CpalOutput {
            queue,
            playout,
            format,
            gate,
            owner: Some(owner),
        }))
    }

    fn input_formats(&self, device: Option<&str>) -> Result<Vec<SupportedFormat>> {
        let device = self.pick_input(device)?;
        let ranges = device
            .supported_input_configs()
            .map_err(|e| classify(Device::Input, &e.to_string()))?;
        Ok(collect_formats(ranges))
    }

    fn output_formats(&self, device: Option<&str>) -> Result<Vec<SupportedFormat>> {
        let device = self.pick_output(device)?;
        let ranges = device
            .supported_output_configs()
            .map_err(|e| classify(Device::Output, &e.to_string()))?;
        Ok(collect_formats(ranges))
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

    fn queued(block: Samples) -> Drain {
        let queue = Arc::new(Ring::new(4));
        queue.push(block);
        Drain::new(queue)
    }

    /// A block larger than one device buffer used to go out with what
    /// did not fit still inside it.
    #[test]
    fn what_did_not_fit_is_handed_over_next_time() {
        let mut drain = queued(Samples::I16((0..512).map(|n| n as i16).collect()));

        let mut first = [0i16; 256];
        drain.fill(&mut first);
        let mut second = [0i16; 256];
        drain.fill(&mut second);

        assert_eq!(first[0], 0, "the first buffer starts at the beginning");
        assert_eq!(second[0], 256, "the rest of the block follows it");
        assert_eq!(second[255], 511, "right to the end of it");
    }

    /// A speaker that speaks neither 16-bit nor float used to be handed
    /// nothing at all.
    #[test]
    fn a_speaker_of_any_spelling_is_handed_audio() {
        for format in [
            SampleFormat::I8,
            SampleFormat::I16,
            SampleFormat::I32,
            SampleFormat::I64,
            SampleFormat::U8,
            SampleFormat::U16,
            SampleFormat::F32,
            SampleFormat::F64,
        ] {
            let mut drain = queued(Samples::I16(vec![16_000; 8]));
            let mut room = vec![0u8; 8 * format.sample_size()];
            // Safe: the buffer is this library's own and long enough.
            let mut data = unsafe { cpal::Data::from_parts(room.as_mut_ptr().cast(), 8, format) };
            fill_from_samples(&mut data, format, &mut drain);
            assert!(
                room.iter().any(|b| *b != 0),
                "{format:?} was handed silence"
            );
        }
    }

    #[test]
    fn a_dry_queue_is_silence_and_not_the_last_thing_played() {
        let mut drain = Drain::new(Arc::new(Ring::new(4)));
        let mut out = [1234i16; 8];
        drain.fill(&mut out);
        assert!(out.iter().all(|s| *s == 0));
    }

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
                wanted: None,
            })
            .expect("open the default microphone");

        println!("device format: {:?}", input.format());
        let block = input.read().expect("audio");
        assert!(!block.is_empty(), "the device produced an empty block");
        input.stop().unwrap();
    }
}
