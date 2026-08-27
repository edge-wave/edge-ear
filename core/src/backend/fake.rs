//! A backend that plays canned audio instead of touching hardware. It
//! drives time itself, so tests run fast and answer the same each run.

use std::sync::{Arc, Mutex};

use super::{AudioBackend, DeviceInfo, FormatRequest, InputStream, OutputStream};
use crate::capture::Samples;
use crate::config::{AudioFormat, Device};
use crate::error::{Error, Result};

/// What the fake microphone will produce, and how it should fail.
#[derive(Debug, Clone, Default)]
pub struct FakeSetup {
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
}

pub struct FakeBackend {
    setup: FakeSetup,
    pub playback: Arc<Mutex<PlaybackLog>>,
}

impl FakeBackend {
    pub fn new(setup: FakeSetup) -> Self {
        Self {
            setup,
            playback: Arc::new(Mutex::new(PlaybackLog::default())),
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
    due: Option<std::time::Instant>,
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
            let now = std::time::Instant::now();
            if *due > now {
                std::thread::sleep(*due - now);
            } else if now - *due > std::time::Duration::from_secs(1) {
                // Further behind than any reader keeps history for.
                *due = now;
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
}

impl OutputStream for FakeOutput {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn write(&mut self, samples: &Samples) -> Result<()> {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        match samples {
            Samples::I16(v) => log.written.extend_from_slice(v),
            Samples::F32(v) => log
                .written
                .extend(v.iter().map(|s| (s * i16::MAX as f32) as i16)),
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).stopped = true;
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
            pace: self.setup.paced.then(|| {
                let format = self.setup.input_format.unwrap_or(AudioFormat::mono_16k());
                let frames = self.setup.block_samples.max(1) / format.channels.max(1) as usize;
                std::time::Duration::from_secs_f64(frames as f64 / format.sample_rate as f64)
            }),
        }))
    }

    fn open_output(&mut self, req: &FormatRequest) -> Result<Box<dyn OutputStream>> {
        if let Some(failure) = self.setup.output_error {
            return Err(failure.to_error(Device::Output));
        }
        Ok(Box::new(FakeOutput {
            format: req.preferred,
            log: Arc::clone(&self.playback),
        }))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> FormatRequest {
        FormatRequest {
            device: None,
            preferred: AudioFormat::mono_16k(),
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
