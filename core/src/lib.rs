//! A local, real-time audio front end.
//!
//! It captures a microphone, spots a wake word, notices when speech has
//! ended, and plays sounds. It does not understand speech, and it never
//! reaches the network.

pub mod backend;
pub mod capture;
pub mod config;
pub mod error;
pub mod events;

use std::sync::{Arc, Mutex, MutexGuard};

use backend::{AudioBackend, DeviceInfo, fake::FakeBackend};
use capture::AudioChunk;
use capture::ring::Ring;
use config::{AudioFormat, Config, Target};
use error::{Error, Result};
use events::Event;
use events::dispatch::{DEFAULT_QUEUE_CAPACITY, Dispatcher};

/// Where a handle is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleState {
    /// Made but not capturing. Formats may still be set here.
    Created,
    /// Capturing. The conversion pipeline is fixed.
    Running,
    /// Finished. Every call fails from here.
    Destroyed,
}

struct Inner {
    state: HandleState,
    config: Config,
}

/// One running instance. Owns one microphone and one speaker.
pub struct EdgeEar {
    inner: Mutex<Inner>,
    dispatcher: Dispatcher,
    /// The application's own view of live audio. Always present, so it
    /// works whether or not any detector is switched on.
    read_ring: Arc<Ring<AudioChunk>>,
    backend: Mutex<Box<dyn AudioBackend>>,
}

impl EdgeEar {
    /// Build a handle over the real audio devices.
    pub fn new() -> Result<Self> {
        // Until the cpal backend lands, the fake one keeps the surface
        // usable and every lifecycle test honest.
        Self::with_backend(Box::new(FakeBackend::silent()))
    }

    /// Build a handle over a supplied backend. This is how tests run
    /// the whole pipeline without hardware.
    pub fn with_backend(backend: Box<dyn AudioBackend>) -> Result<Self> {
        let config = Config::default();
        config.validate()?;

        Ok(Self {
            inner: Mutex::new(Inner {
                state: HandleState::Created,
                config,
            }),
            dispatcher: Dispatcher::new(DEFAULT_QUEUE_CAPACITY),
            read_ring: Arc::new(Ring::new(64)),
            backend: Mutex::new(backend),
        })
    }

    // ── lifecycle ────────────────────────────────────────────────────

    pub fn start(&self) -> Result<()> {
        let mut inner = self.lock();
        match inner.state {
            HandleState::Destroyed => Err(Error::Destroyed),
            HandleState::Running => Err(Error::AlreadyRunning),
            HandleState::Created => {
                inner.config.validate()?;
                inner.state = HandleState::Running;
                self.read_ring.reopen();
                Ok(())
            }
        }
    }

    pub fn stop(&self) -> Result<()> {
        let mut inner = self.lock();
        match inner.state {
            HandleState::Destroyed => Err(Error::Destroyed),
            HandleState::Created => Err(Error::NotRunning),
            HandleState::Running => {
                inner.state = HandleState::Created;
                drop(inner);
                // Release anyone waiting on a read, rather than leaving
                // them blocked until the process ends.
                self.read_ring.close();
                Ok(())
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.lock().state == HandleState::Running
    }

    pub fn state(&self) -> HandleState {
        self.lock().state
    }

    /// Stop everything and release both devices.
    ///
    /// `Drop` calls this too, so an application does not have to.
    pub fn destroy(&self) {
        {
            let mut inner = self.lock();
            if inner.state == HandleState::Destroyed {
                return;
            }
            inner.state = HandleState::Destroyed;
        }
        self.read_ring.close();
        self.dispatcher.shutdown();
    }

    // ── events ───────────────────────────────────────────────────────

    /// Set the handler for every notification.
    ///
    /// It runs on the dispatcher thread. A slow handler delays later
    /// events and nothing else.
    pub fn on_event(&self, handler: impl Fn(Event) + Send + Sync + 'static) -> Result<()> {
        self.alive()?;
        self.dispatcher.set_handler(Box::new(handler));
        Ok(())
    }

    // ── configuration ────────────────────────────────────────────────

    /// Set the audio format one consumer receives.
    ///
    /// Only before capture starts: once it is running the conversion
    /// pipeline is built and changing it would mean rebuilding it.
    pub fn set_format(&self, target: Target, format: AudioFormat) -> Result<()> {
        format.validate_for(target)?;
        let mut inner = self.stopped_only("the audio format")?;
        match target {
            Target::Wake => inner.config.fixed.wake_format = format,
            Target::Speech => inner.config.fixed.speech_format = format,
            Target::Read => inner.config.fixed.read_format = format,
        }
        Ok(())
    }

    pub fn set_input_device(&self, name: Option<&str>) -> Result<()> {
        let mut inner = self.stopped_only("the input device")?;
        inner.config.fixed.input_device = name.map(str::to_string);
        Ok(())
    }

    pub fn set_output_device(&self, name: Option<&str>) -> Result<()> {
        let mut inner = self.stopped_only("the output device")?;
        inner.config.fixed.output_device = name.map(str::to_string);
        Ok(())
    }

    /// Change one setting, keeping the old value if the new one is bad.
    fn tune(&self, apply: impl FnOnce(&mut Config)) -> Result<()> {
        let mut inner = self.alive_mut()?;
        let mut candidate = inner.config.clone();
        apply(&mut candidate);
        candidate.validate()?;
        inner.config = candidate;
        Ok(())
    }

    pub fn set_wake_threshold(&self, value: f32) -> Result<()> {
        self.tune(|c| c.tunable.wake_threshold = value)
    }

    pub fn set_speech_threshold(&self, value: f32) -> Result<()> {
        self.tune(|c| c.tunable.speech_threshold = value)
    }

    pub fn set_silence_duration(&self, value: std::time::Duration) -> Result<()> {
        self.tune(|c| c.tunable.silence_duration = value)
    }

    pub fn set_max_recording(&self, value: std::time::Duration) -> Result<()> {
        self.tune(|c| c.tunable.max_recording = value)
    }

    pub fn set_no_speech_timeout(&self, value: std::time::Duration) -> Result<()> {
        self.tune(|c| c.tunable.no_speech_timeout = value)
    }

    pub fn set_pre_roll(&self, value: std::time::Duration) -> Result<()> {
        self.tune(|c| c.tunable.pre_roll = value)
    }

    pub fn config(&self) -> Config {
        self.lock().config.clone()
    }

    // ── devices ──────────────────────────────────────────────────────

    pub fn input_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.alive()?;
        self.backend_lock().input_devices()
    }

    pub fn output_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.alive()?;
        self.backend_lock().output_devices()
    }

    // ── internals ────────────────────────────────────────────────────

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn backend_lock(&self) -> MutexGuard<'_, Box<dyn AudioBackend>> {
        self.backend.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn alive(&self) -> Result<()> {
        match self.lock().state {
            HandleState::Destroyed => Err(Error::Destroyed),
            _ => Ok(()),
        }
    }

    fn alive_mut(&self) -> Result<MutexGuard<'_, Inner>> {
        let inner = self.lock();
        match inner.state {
            HandleState::Destroyed => Err(Error::Destroyed),
            _ => Ok(inner),
        }
    }

    fn stopped_only(&self, what: &'static str) -> Result<MutexGuard<'_, Inner>> {
        let inner = self.lock();
        match inner.state {
            HandleState::Destroyed => Err(Error::Destroyed),
            HandleState::Running => Err(Error::RunningNotAllowed { what }),
            HandleState::Created => Ok(inner),
        }
    }
}

impl Drop for EdgeEar {
    fn drop(&mut self) {
        self.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::SampleType;
    use std::time::Duration;

    fn ear() -> EdgeEar {
        EdgeEar::with_backend(Box::new(FakeBackend::silent())).unwrap()
    }

    #[test]
    fn a_new_handle_is_not_running() {
        let ear = ear();
        assert_eq!(ear.state(), HandleState::Created);
        assert!(!ear.is_running());
    }

    #[test]
    fn start_then_stop_returns_to_created() {
        let ear = ear();
        ear.start().unwrap();
        assert!(ear.is_running());
        ear.stop().unwrap();
        assert_eq!(ear.state(), HandleState::Created);
    }

    #[test]
    fn start_and_stop_may_repeat_over_one_handle() {
        let ear = ear();
        for _ in 0..3 {
            ear.start().unwrap();
            ear.stop().unwrap();
        }
    }

    #[test]
    fn starting_twice_says_so() {
        let ear = ear();
        ear.start().unwrap();
        assert!(matches!(ear.start().unwrap_err(), Error::AlreadyRunning));
    }

    #[test]
    fn stopping_twice_says_so() {
        let ear = ear();
        ear.start().unwrap();
        ear.stop().unwrap();
        assert!(matches!(ear.stop().unwrap_err(), Error::NotRunning));
    }

    #[test]
    fn every_call_fails_once_destroyed() {
        let ear = ear();
        ear.destroy();
        assert!(matches!(ear.start().unwrap_err(), Error::Destroyed));
        assert!(matches!(ear.stop().unwrap_err(), Error::Destroyed));
        assert!(matches!(
            ear.set_wake_threshold(0.5).unwrap_err(),
            Error::Destroyed
        ));
        assert!(matches!(ear.input_devices().unwrap_err(), Error::Destroyed));
    }

    #[test]
    fn destroying_twice_is_harmless() {
        let ear = ear();
        ear.destroy();
        ear.destroy();
    }

    #[test]
    fn formats_cannot_change_once_running() {
        let ear = ear();
        ear.set_format(Target::Read, AudioFormat::new(44_100, 2, SampleType::F32))
            .unwrap();
        ear.start().unwrap();

        let err = ear
            .set_format(Target::Read, AudioFormat::mono_16k())
            .unwrap_err();
        assert!(matches!(err, Error::RunningNotAllowed { .. }), "{err}");
        assert!(err.to_string().contains("stop capture first"), "{err}");
    }

    #[test]
    fn a_format_the_model_cannot_take_is_refused() {
        let ear = ear();
        let err = ear
            .set_format(Target::Wake, AudioFormat::new(48_000, 1, SampleType::I16))
            .unwrap_err();
        assert!(matches!(err, Error::UnsupportedFormat { .. }), "{err}");
    }

    #[test]
    fn thresholds_change_at_any_time() {
        let ear = ear();
        ear.set_wake_threshold(0.8).unwrap();
        ear.start().unwrap();
        ear.set_wake_threshold(0.2).unwrap();
        assert_eq!(ear.config().tunable.wake_threshold, 0.2);
    }

    #[test]
    fn a_rejected_value_leaves_the_previous_one_alone() {
        let ear = ear();
        ear.set_wake_threshold(0.7).unwrap();
        assert!(ear.set_wake_threshold(9.0).is_err());
        assert_eq!(ear.config().tunable.wake_threshold, 0.7);
    }

    #[test]
    fn pre_roll_beyond_the_history_is_refused() {
        let ear = ear();
        let err = ear.set_pre_roll(Duration::from_secs(30)).unwrap_err();
        assert!(matches!(err, Error::InvalidValue { .. }), "{err}");
    }

    #[test]
    fn devices_are_listed_by_name() {
        let ear = ear();
        let inputs = ear.input_devices().unwrap();
        assert!(inputs.iter().any(|d| d.is_default));
    }

    #[test]
    fn the_handle_is_usable_from_several_threads() {
        let ear = Arc::new(ear());
        let mut threads = Vec::new();
        for _ in 0..8 {
            let ear = Arc::clone(&ear);
            threads.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    let _ = ear.set_wake_threshold(0.5);
                    let _ = ear.is_running();
                    let _ = ear.config();
                }
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
    }
}
