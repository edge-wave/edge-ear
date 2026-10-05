//! A backend that plays canned audio instead of touching hardware. It
//! drives time itself, so tests run fast and answer the same each run.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::playout::{FADE_OUT, Playout};
use super::{AudioBackend, DeviceInfo, FormatRequest, InputStream, OutputStream, SupportedFormat};
use crate::capture::Samples;
use crate::config::{AudioFormat, Device};
use crate::echo::EchoReference;
use crate::error::{Error, Result};

/// What the fake microphone will produce, and how it should fail.
#[derive(Debug, Clone, Default)]
pub struct FakeSetup {
    /// What the fake devices say they will take. Empty means "only
    /// the format they are set to", which is what a plain device does.
    pub input_formats: Vec<SupportedFormat>,
    pub output_formats: Vec<SupportedFormat>,
    /// Audio handed out one block at a time. When it runs out the
    /// stream reports silence forever.
    pub input_audio: Vec<i16>,
    pub input_format: Option<AudioFormat>,
    pub block_samples: usize,
    pub input_devices: Vec<DeviceInfo>,
    pub output_devices: Vec<DeviceInfo>,
    /// Set to make `open_input` fail, so permission and missing-device
    /// paths can be tested.
    pub input_error: Option<FakeFailure>,
    pub output_error: Option<FakeFailure>,
    /// A device that is open but hands over nothing. Lets a test check
    /// what a reader does while it waits.
    pub starve: bool,
    /// Hand blocks over at the speed a real device would. Any test
    /// asking whether something keeps up needs this, because keeping up
    /// means nothing against a device running flat out.
    pub paced: bool,
    /// How long after the fake speaker takes audio it is heard, the way
    /// a real device's own buffering delays it.
    pub output_delay: Duration,
    /// What the speaker opens at when left to choose, one per open, the
    /// last repeating. Empty means mono 16 kHz, as before.
    pub output_defaults: Vec<AudioFormat>,
    /// The speaker heard back by a paced microphone at this gain, as in a real room.
    pub echo_gain: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeFailure {
    NoDevice,
    PermissionDenied,
    DeviceLost,
}

impl FakeFailure {
    fn to_error(self, device: Device) -> Error {
        match self {
            FakeFailure::NoDevice => Error::NoDevice(device),
            FakeFailure::PermissionDenied => Error::PermissionDenied,
            FakeFailure::DeviceLost => Error::DeviceLost(device),
        }
    }
}

/// Everything the fake speaker was asked to play, for tests to inspect.
#[derive(Debug, Default)]
pub struct PlaybackLog {
    pub written: Vec<i16>,
    pub stopped: bool,
    /// How often the speaker was opened and let go. Releasing an idle
    /// device shows up nowhere else.
    pub opens: usize,
    pub closes: usize,
    /// The format each open asked for, `None` when the device chose.
    pub requested: Vec<Option<AudioFormat>>,
    /// How often the speaker was told to drop what it had not played.
    pub flushes: usize,
}

pub struct FakeBackend {
    setup: FakeSetup,
    pub playback: Arc<Mutex<PlaybackLog>>,
    room: Arc<Mutex<Room>>,
}

/// What the speaker has played and when, for the microphone to hear back.
#[derive(Default)]
struct Room {
    /// When each block starts to be heard, at what rate, as one channel.
    heard: Vec<(Instant, u32, Vec<f32>)>,
}

impl Room {
    /// The speaker at `at`, or silence if it was not playing then.
    fn at(&self, at: Instant) -> f32 {
        for (start, rate, samples) in &self.heard {
            let Some(offset) = at.checked_duration_since(*start) else {
                continue;
            };
            let index = (offset.as_secs_f64() * f64::from(*rate)) as usize;
            if let Some(sample) = samples.get(index) {
                return *sample;
            }
        }
        0.0
    }

    /// Play a block from `start`, right after the last if it follows within a few milliseconds,
    /// because a real speaker plays one sample after another whatever its thread's timing.
    fn play(&mut self, start: Instant, rate: u32, samples: Vec<f32>) {
        let start = match self.heard.last() {
            Some((last, last_rate, last_samples)) => {
                let end = *last
                    + Duration::from_secs_f64(last_samples.len() as f64 / f64::from(*last_rate));
                if start >= end && start - end < Duration::from_millis(5) {
                    end
                } else {
                    start
                }
            }
            None => start,
        };
        self.heard.push((start, rate, samples));
    }

    /// Forget what was heard before `before`, which no microphone block can reach any more.
    fn forget(&mut self, before: Instant) {
        self.heard.retain(|(start, rate, samples)| {
            *start + Duration::from_secs_f64(samples.len() as f64 / f64::from(*rate)) >= before
        });
    }
}

impl FakeBackend {
    pub fn new(setup: FakeSetup) -> Self {
        Self {
            setup,
            playback: Arc::new(Mutex::new(PlaybackLog::default())),
            room: Arc::new(Mutex::new(Room::default())),
        }
    }

    /// A microphone that produces endless silence. Enough for lifecycle
    /// and independence tests that do not care what the audio says.
    pub fn silent() -> Self {
        Self::new(FakeSetup {
            input_audio: Vec::new(),
            input_format: Some(AudioFormat::mono_16k()),
            block_samples: 160,
            input_devices: vec![DeviceInfo {
                id: "fake:input:0".to_string(),
                name: "fake input".to_string(),
                is_default: true,
            }],
            output_devices: vec![DeviceInfo {
                id: "fake:output:0".to_string(),
                name: "fake output".to_string(),
                is_default: true,
            }],
            ..Default::default()
        })
    }

    /// A microphone that plays the given samples once, then silence.
    pub fn playing(audio: Vec<i16>, format: AudioFormat, block_samples: usize) -> Self {
        let mut backend = Self::silent();
        backend.setup.input_audio = audio;
        backend.setup.input_format = Some(format);
        backend.setup.block_samples = block_samples;
        backend
    }

    pub fn failing_input(failure: FakeFailure) -> Self {
        let mut backend = Self::silent();
        backend.setup.input_error = Some(failure);
        backend
    }

    /// A microphone that opens but never produces audio.
    pub fn starving() -> Self {
        let mut backend = Self::silent();
        backend.setup.starve = true;
        backend
    }

    /// A speaker heard only `delay` after it takes audio, as a real one is.
    pub fn delayed_output(delay: Duration) -> Self {
        let mut backend = Self::silent();
        backend.setup.output_delay = delay;
        backend
    }

    /// Silence, delivered at the speed a real device would.
    pub fn paced() -> Self {
        let mut backend = Self::silent();
        backend.setup.paced = true;
        backend
    }

    pub fn failing_output(failure: FakeFailure) -> Self {
        let mut backend = Self::silent();
        backend.setup.output_error = Some(failure);
        backend
    }
}

struct FakeInput {
    audio: Vec<i16>,
    cursor: usize,
    block: usize,
    format: AudioFormat,
    stopped: bool,
    starve: bool,
    /// How long one block covers, waited out before handing it over.
    pace: Option<std::time::Duration>,
    /// When the next block is due. Absolute, so a late block does not
    /// push every block after it later still.
    due: Option<Instant>,
    /// The room and how loud the speaker is heard in it, when it is.
    echo: Option<(Arc<Mutex<Room>>, f32)>,
}

impl InputStream for FakeInput {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn read(&mut self) -> Result<Samples> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        if self.starve {
            // Nothing to hand over. Return an empty block after a short
            // pause, so the capture loop keeps checking whether it has
            // been told to stop instead of parking for ever.
            std::thread::sleep(std::time::Duration::from_millis(5));
            return Ok(Samples::I16(Vec::new()));
        }
        let end = (self.cursor + self.block).min(self.audio.len());
        let mut block: Vec<i16> = self.audio[self.cursor.min(end)..end].to_vec();
        self.cursor = end;
        // Past the end of the canned audio, keep producing silence so a
        // test can run as long as it likes.
        block.resize(self.block, 0);
        if let Some(pace) = self.pace {
            // A real device buffers while the machine is busy instead
            // of slowing down, so catch up rather than sleeping again.
            let due = self.due.get_or_insert_with(std::time::Instant::now);
            *due += pace;
            let now = Instant::now();
            if *due > now {
                std::thread::sleep(*due - now);
            } else if now - *due > std::time::Duration::from_secs(1) {
                // Further behind than any reader keeps history for.
                *due = now;
            }
            if let Some((room, gain)) = &self.echo {
                let start = *due - pace;
                let mut room = room.lock().unwrap_or_else(|e| e.into_inner());
                room.forget(start);
                let channels = usize::from(self.format.channels.max(1));
                let rate = f64::from(self.format.sample_rate);
                for (n, sample) in block.iter_mut().enumerate() {
                    let at = start + Duration::from_secs_f64((n / channels) as f64 / rate);
                    let heard = f32::from(*sample) + room.at(at) * gain * 32_767.0;
                    *sample = heard.clamp(-32_768.0, 32_767.0) as i16;
                }
            }
        }
        Ok(Samples::I16(block))
    }

    fn stop(&mut self) -> Result<()> {
        self.stopped = true;
        Ok(())
    }
}

struct FakeOutput {
    format: AudioFormat,
    log: Arc<Mutex<PlaybackLog>>,
    playout: Playout,
    delay: Duration,
    /// When what was taken so far will have played out, the way a real
    /// speaker plays one block after another.
    heard_until: Option<Instant>,
    /// Set when the speaker should take as long as a real one, on the
    /// same absolute schedule the microphone keeps.
    paced: bool,
    due: Option<Instant>,
    room: Option<Arc<Mutex<Room>>>,
}

impl OutputStream for FakeOutput {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn write(&mut self, samples: &Samples) -> Result<()> {
        if self.paced {
            let frames = samples.len() / self.format.channels.max(1) as usize;
            let span = Duration::from_secs_f64(frames as f64 / self.format.sample_rate as f64);
            let now = Instant::now();
            let due = self.due.get_or_insert(now);
            // Idle between sounds, or late. Either way a real speaker
            // has run dry, so begin again rather than catching up.
            if *due < now {
                *due = now;
            }
            *due += span;
            std::thread::sleep(*due - now);
        }
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        match samples {
            Samples::I16(v) => log.written.extend_from_slice(v),
            Samples::F32(v) => log
                .written
                .extend(v.iter().map(|s| (s * i16::MAX as f32) as i16)),
        }
        let now = Instant::now();
        let start = self
            .heard_until
            .map_or(now + self.delay, |until| until.max(now + self.delay));
        let frames = samples.len() / usize::from(self.format.channels.max(1));
        self.heard_until = Some(
            start + Duration::from_secs_f64(frames as f64 / f64::from(self.format.sample_rate)),
        );
        self.playout.took(samples.len(), start - now);
        self.playout.played(start - now, || samples.clone());
        if let Some(room) = &self.room {
            let channels = usize::from(self.format.channels.max(1));
            let mono = match samples {
                Samples::I16(v) => v
                    .iter()
                    .step_by(channels)
                    .map(|s| f32::from(*s) / 32_768.0)
                    .collect(),
                Samples::F32(v) => v.iter().step_by(channels).copied().collect(),
            };
            let mut room = room.lock().unwrap_or_else(|e| e.into_inner());
            room.play(start, self.format.sample_rate, mono);
        }
        Ok(())
    }

    fn heard_at(&self, position: u64) -> Option<Instant> {
        self.playout.heard_at(position)
    }

    fn flush(&mut self) -> u64 {
        // Counted as taken when written, so nothing is returned; past the fade,
        // what had not played out yet is simply never heard.
        self.log.lock().unwrap_or_else(|e| e.into_inner()).flushes += 1;
        let soonest = Instant::now() + self.delay + FADE_OUT;
        self.heard_until = self.heard_until.map(|until| until.min(soonest));
        0
    }

    fn tap(&mut self, reference: Arc<EchoReference>) -> bool {
        self.playout.set_tap(reference);
        true
    }

    fn stop(&mut self) -> Result<()> {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.stopped = true;
        log.closes += 1;
        Ok(())
    }
}

impl AudioBackend for FakeBackend {
    fn open_input(&mut self, _req: &FormatRequest) -> Result<Box<dyn InputStream>> {
        if let Some(failure) = self.setup.input_error {
            return Err(failure.to_error(Device::Input));
        }
        Ok(Box::new(FakeInput {
            audio: self.setup.input_audio.clone(),
            cursor: 0,
            block: self.setup.block_samples.max(1),
            format: self.setup.input_format.unwrap_or(AudioFormat::mono_16k()),
            stopped: false,
            starve: self.setup.starve,
            due: None,
            echo: self
                .setup
                .echo_gain
                .map(|gain| (Arc::clone(&self.room), gain)),
            pace: self.setup.paced.then(|| {
                let format = self.setup.input_format.unwrap_or(AudioFormat::mono_16k());
                let frames = self.setup.block_samples.max(1) / format.channels.max(1) as usize;
                Duration::from_secs_f64(frames as f64 / format.sample_rate as f64)
            }),
        }))
    }

    fn open_output(&mut self, req: &FormatRequest) -> Result<Box<dyn OutputStream>> {
        if let Some(failure) = self.setup.output_error {
            return Err(failure.to_error(Device::Output));
        }
        let opened = {
            let mut log = self.playback.lock().unwrap_or_else(|e| e.into_inner());
            log.opens += 1;
            log.requested.push(req.wanted);
            log.opens - 1
        };
        let chosen = self
            .setup
            .output_defaults
            .get(opened)
            .or(self.setup.output_defaults.last())
            .copied()
            .unwrap_or(AudioFormat::mono_16k());
        let format = req.wanted.unwrap_or(chosen);
        Ok(Box::new(FakeOutput {
            paced: self.setup.paced,
            due: None,
            format,
            log: Arc::clone(&self.playback),
            playout: Playout::new(format),
            delay: self.setup.output_delay,
            heard_until: None,
            room: self.setup.echo_gain.map(|_| Arc::clone(&self.room)),
        }))
    }

    fn input_formats(&self, _device: Option<&str>) -> Result<Vec<SupportedFormat>> {
        Ok(offered(
            &self.setup.input_formats,
            self.setup.input_format.unwrap_or(AudioFormat::mono_16k()),
        ))
    }

    fn output_formats(&self, _device: Option<&str>) -> Result<Vec<SupportedFormat>> {
        Ok(offered(
            &self.setup.output_formats,
            self.setup.input_format.unwrap_or(AudioFormat::mono_16k()),
        ))
    }

    fn input_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(self.setup.input_devices.clone())
    }

    fn output_devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(self.setup.output_devices.clone())
    }

    fn describe(&self) -> &'static str {
        "fake"
    }
}

/// What was set, or the one format the device is fixed at.
fn offered(set: &[SupportedFormat], fallback: AudioFormat) -> Vec<SupportedFormat> {
    if !set.is_empty() {
        return set.to_vec();
    }
    vec![SupportedFormat {
        channels: fallback.channels,
        min_sample_rate: fallback.sample_rate,
        max_sample_rate: fallback.sample_rate,
        sample_type: fallback.sample_type,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SampleType;

    #[test]
    fn a_device_offers_what_it_was_set_to() {
        let backend = FakeBackend::silent();
        let offered = backend.input_formats(None).expect("formats");
        assert_eq!(
            offered,
            vec![SupportedFormat {
                channels: 1,
                min_sample_rate: 16_000,
                max_sample_rate: 16_000,
                sample_type: SampleType::I16,
            }]
        );
    }

    #[test]
    fn a_fussy_device_offers_exactly_what_it_was_given() {
        let wanted = vec![SupportedFormat {
            channels: 2,
            min_sample_rate: 44_100,
            max_sample_rate: 48_000,
            sample_type: SampleType::F32,
        }];
        let mut backend = FakeBackend::silent();
        backend.setup.output_formats = wanted.clone();
        assert_eq!(backend.output_formats(None).expect("formats"), wanted);
    }

    #[test]
    fn a_range_says_what_falls_inside_it() {
        let range = SupportedFormat {
            channels: 1,
            min_sample_rate: 16_000,
            max_sample_rate: 48_000,
            sample_type: SampleType::I16,
        };
        assert!(range.covers(&AudioFormat::mono_16k()));
        assert!(!range.covers(&AudioFormat::new(8_000, 1, SampleType::I16)));
        assert!(!range.covers(&AudioFormat::new(16_000, 2, SampleType::I16)));
        assert!(!range.covers(&AudioFormat::new(16_000, 1, SampleType::F32)));
    }

    fn request() -> FormatRequest {
        FormatRequest {
            device: None,
            wanted: None,
        }
    }

    #[test]
    fn canned_audio_comes_back_in_blocks() {
        let mut backend = FakeBackend::playing((1..=10).collect(), AudioFormat::mono_16k(), 4);
        let mut input = backend.open_input(&request()).unwrap();

        assert_eq!(input.read().unwrap(), Samples::I16(vec![1, 2, 3, 4]));
        assert_eq!(input.read().unwrap(), Samples::I16(vec![5, 6, 7, 8]));
        // The tail is padded, then it is silence from here on.
        assert_eq!(input.read().unwrap(), Samples::I16(vec![9, 10, 0, 0]));
        assert_eq!(input.read().unwrap(), Samples::I16(vec![0, 0, 0, 0]));
    }

    #[test]
    fn a_refused_microphone_is_its_own_error() {
        let mut backend = FakeBackend::failing_input(FakeFailure::PermissionDenied);
        let err = backend
            .open_input(&request())
            .err()
            .expect("open must fail");
        assert!(matches!(err, Error::PermissionDenied), "{err}");
    }

    #[test]
    fn a_missing_microphone_is_a_different_error() {
        let mut backend = FakeBackend::failing_input(FakeFailure::NoDevice);
        let err = backend
            .open_input(&request())
            .err()
            .expect("open must fail");
        assert!(matches!(err, Error::NoDevice(Device::Input)), "{err}");
    }

    #[test]
    fn a_missing_speaker_is_reported_against_the_output_device() {
        let mut backend = FakeBackend::failing_output(FakeFailure::NoDevice);
        let err = backend
            .open_output(&request())
            .err()
            .expect("open must fail");
        assert!(matches!(err, Error::NoDevice(Device::Output)), "{err}");
    }

    #[test]
    fn playback_is_recorded_for_inspection() {
        let mut backend = FakeBackend::silent();
        let log = Arc::clone(&backend.playback);
        let mut out = backend.open_output(&request()).unwrap();
        out.write(&Samples::I16(vec![1, 2, 3])).unwrap();
        assert_eq!(log.lock().unwrap().written, vec![1, 2, 3]);
    }
}
