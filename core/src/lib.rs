//! A local, real-time audio front end.
//!
//! It captures a microphone, spots a wake word, notices when speech has
//! ended, and plays sounds. It does not understand speech, and it never
//! reaches the network.

pub mod backend;
pub mod config;
pub mod error;
pub mod events;

// Machinery, not surface. An application never builds a capture thread
// or a player itself; it drives them through the handle below. Keeping
// these crate-only is what makes "one owner per device" a rule the
// compiler holds, rather than one the documentation asks for.
pub(crate) mod capture;
pub(crate) mod player;
pub(crate) mod speech;

// The few types from those modules that an application does touch.
pub use capture::{AudioChunk, Samples};
pub use player::registry::{SoundId, SoundSource};

use std::sync::{Arc, Mutex, MutexGuard};

use std::time::Duration;

use backend::{AudioBackend, DeviceInfo, FormatRequest};
use capture::{CaptureThread, Consumer, ConsumerKind};
use config::{AudioFormat, Config, Target};
use error::{Error, Result};
use events::Event;
use events::dispatch::{DEFAULT_QUEUE_CAPACITY, Dispatcher};
use player::Player;
use player::registry::Registry;
use speech::SpeechThread;

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
    capture: Option<CaptureThread>,
    /// Live only while capturing. Rebuilt on every start, because the
    /// conversion pipeline is fixed when the thread comes up.
    consumers: Vec<Consumer>,
    /// The one owner of the speaker. Opened when the first sound is
    /// wanted, not at start, so an application that never plays
    /// anything never claims the device.
    player: Option<Player>,
    sounds: Registry,
    /// Runs while capture runs. Detection happens on its own thread, so
    /// inference never stalls the audio path.
    speech: Option<SpeechThread>,
    /// Which detectors the application has asked for. Kept apart from
    /// the consumers because a detector may be switched on before there
    /// is anything to switch, and that intent must survive until start.
    wake_wanted: bool,
    speech_wanted: bool,
}

impl Inner {
    fn consumer(&self, kind: ConsumerKind) -> Option<&Consumer> {
        self.consumers.iter().find(|c| c.kind == kind)
    }
}

/// One running instance. Owns one microphone and one speaker.
pub struct EdgeEar {
    inner: Mutex<Inner>,
    dispatcher: Arc<Dispatcher>,
    backend: Mutex<Box<dyn AudioBackend>>,
}

impl EdgeEar {
    /// Build a handle over the real audio devices.
    #[cfg(feature = "cpal-backend")]
    pub fn new() -> Result<Self> {
        Self::with_backend(Box::new(backend::cpal_backend::CpalBackend::new()))
    }

    /// Without a device backend compiled in there is nothing to open,
    /// so a caller must supply one.
    #[cfg(not(feature = "cpal-backend"))]
    pub fn new() -> Result<Self> {
        Err(Error::NoDevice(config::Device::Input))
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
                capture: None,
                consumers: Vec::new(),
                player: None,
                sounds: Registry::new(),
                speech: None,
                wake_wanted: false,
                speech_wanted: false,
            }),
            dispatcher: Arc::new(Dispatcher::new(DEFAULT_QUEUE_CAPACITY)),
            backend: Mutex::new(backend),
        })
    }

    // ── lifecycle ────────────────────────────────────────────────────

    pub fn start(&self) -> Result<()> {
        let mut inner = self.lock();
        match inner.state {
            HandleState::Destroyed => return Err(Error::Destroyed),
            HandleState::Running => return Err(Error::AlreadyRunning),
            HandleState::Created => {}
        }
        inner.config.validate()?;

        let request = FormatRequest {
            device: inner.config.fixed.input_device.clone(),
            preferred: inner.config.fixed.read_format,
        };
        let stream = self.backend_lock().open_input(&request)?;

        let consumers = build_consumers(&inner.config, inner.wake_wanted, inner.speech_wanted);
        let capture =
            CaptureThread::start(stream, consumers.clone(), Arc::clone(&self.dispatcher))?;

        let speech_ring = consumers
            .iter()
            .find(|c| c.kind == ConsumerKind::Speech)
            .map(|c| Arc::clone(&c.ring))
            .expect("a speech consumer is always built");
        let speech = SpeechThread::start(
            speech_ring,
            inner.config.fixed.speech_format,
            inner.config.tunable.clone(),
            Arc::clone(&self.dispatcher),
        )?;

        inner.consumers = consumers;
        inner.capture = Some(capture);
        inner.speech = Some(speech);
        inner.state = HandleState::Running;
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        let mut inner = self.lock();
        match inner.state {
            HandleState::Destroyed => Err(Error::Destroyed),
            HandleState::Created => Err(Error::NotRunning),
            HandleState::Running => {
                inner.state = HandleState::Created;
                // Stopping closes every ring, which releases anyone
                // waiting on a read rather than leaving them blocked.
                if let Some(mut capture) = inner.capture.take() {
                    capture.stop();
                }
                if let Some(mut speech) = inner.speech.take() {
                    speech.shutdown();
                }
                inner.consumers.clear();
                Ok(())
            }
        }
    }

    // ── reading live audio ───────────────────────────────────────────

    /// Take the next block of live audio.
    ///
    /// Works between `start` and `stop` whether or not any detector is
    /// switched on. `None` waits until audio arrives or capture stops.
    pub fn read(&self, timeout: Option<Duration>) -> Result<AudioChunk> {
        let ring = {
            let inner = self.lock();
            match inner.state {
                HandleState::Destroyed => return Err(Error::Destroyed),
                HandleState::Created => return Err(Error::NotRunning),
                HandleState::Running => {}
            }
            let consumer = inner
                .consumer(ConsumerKind::Read)
                .ok_or(Error::NotRunning)?;
            Arc::clone(&consumer.ring)
        };

        // The handle lock is released before waiting, so stop can run
        // while a reader is parked here.
        let taken = ring.take(timeout)?;
        let mut chunk = taken.item;
        chunk.dropped_before = taken.dropped_before;
        Ok(chunk)
    }

    // ── speech detection ─────────────────────────────────────────────

    /// Start listening for the end of speech.
    ///
    /// Works before or after capture starts, and takes effect on the
    /// next block of audio.
    pub fn enable_speech(&self) -> Result<()> {
        self.set_consumer(ConsumerKind::Speech, true)
    }

    pub fn disable_speech(&self) -> Result<()> {
        self.set_consumer(ConsumerKind::Speech, false)
    }

    pub fn is_speech_enabled(&self) -> bool {
        self.lock().speech_wanted
    }

    /// Open a recording now, without waiting for a wake word.
    ///
    /// One `SpeechEnded` follows, carrying the audio and why it ended.
    pub fn start_recording(&self) -> Result<()> {
        let inner = self.running_only()?;
        let speech = inner.speech.as_ref().ok_or(Error::NotRunning)?;
        speech.set_limits(inner.config.tunable.clone());

        // Pre-roll reaches back into audio already collected. Off by
        // default, because that stretch may hold an alert sound.
        let pre_roll = self.recent_audio(&inner);
        speech.open_recording(pre_roll, true);
        Ok(())
    }

    /// End the open recording. One `SpeechEnded` follows, saying it was
    /// stopped rather than that the speaker went quiet.
    pub fn stop_recording(&self) -> Result<()> {
        let inner = self.running_only()?;
        inner
            .speech
            .as_ref()
            .ok_or(Error::NotRunning)?
            .stop_recording();
        Ok(())
    }

    /// Audio from just before now, as much as pre-roll asks for and the
    /// history holds. A history shorter than that yields what exists.
    fn recent_audio(&self, inner: &Inner) -> Vec<i16> {
        let wanted = inner.config.tunable.pre_roll;
        if wanted.is_zero() {
            return Vec::new();
        }
        let Some(consumer) = inner.consumer(ConsumerKind::Speech) else {
            return Vec::new();
        };
        let format = consumer.format;
        let per_chunk_secs = |chunk: &AudioChunk| chunk.duration().as_secs_f64();

        let mut held = consumer.ring.snapshot();
        let mut taken: Vec<i16> = Vec::new();
        let mut seconds = 0.0;
        while let Some(chunk) = held.pop() {
            if seconds >= wanted.as_secs_f64() {
                break;
            }
            seconds += per_chunk_secs(&chunk);
            if let Some(samples) = chunk.samples.as_i16() {
                let mut front = samples.to_vec();
                front.extend_from_slice(&taken);
                taken = front;
            }
        }
        let _ = format;
        taken
    }

    /// Turn one consumer on or off. Takes effect on the next block and
    /// disturbs no other consumer.
    fn set_consumer(&self, kind: ConsumerKind, on: bool) -> Result<()> {
        let mut inner = self.alive_mut()?;
        match kind {
            ConsumerKind::Wake => inner.wake_wanted = on,
            ConsumerKind::Speech => inner.speech_wanted = on,
            ConsumerKind::Read => {}
        }
        if let Some(consumer) = inner.consumer(kind) {
            consumer.set_enabled(on);
        }
        Ok(())
    }

    fn running_only(&self) -> Result<MutexGuard<'_, Inner>> {
        let inner = self.lock();
        match inner.state {
            HandleState::Destroyed => Err(Error::Destroyed),
            HandleState::Created => Err(Error::NotRunning),
            HandleState::Running => Ok(inner),
        }
    }

    // ── sound playback ───────────────────────────────────────────────

    /// Register a sound so it can be played later.
    ///
    /// Allowed at any time, including while running. It adds an asset
    /// and changes no pipeline, which is what lets a spoken reply that
    /// arrives at run time be played without a restart.
    pub fn register_sound(&self, id: &str, source: SoundSource, volume: f32) -> Result<()> {
        let output = self.ensure_player()?;
        let mut inner = self.alive_mut()?;
        inner
            .sounds
            .register(id.to_string(), source, volume, output)
    }

    /// Forget a sound and release the audio it held.
    pub fn unregister_sound(&self, id: &str) -> Result<()> {
        let mut inner = self.alive_mut()?;
        inner.sounds.unregister(id)
    }

    /// Play a registered sound, optionally repeating until stopped.
    /// A newer sound supersedes whatever was playing.
    pub fn play_sound(&self, id: &str, repeat: bool) -> Result<()> {
        self.ensure_player()?;
        let inner = self.alive_mut()?;
        let sound = inner.sounds.get(id)?.clone();
        let player = inner
            .player
            .as_ref()
            .ok_or(Error::NoDevice(config::Device::Output))?;
        player.play(sound, repeat);
        Ok(())
    }

    /// Cut playback short. No completion event follows, because the
    /// sound did not end on its own.
    pub fn stop_sound(&self) -> Result<()> {
        let inner = self.alive_mut()?;
        if let Some(player) = inner.player.as_ref() {
            player.stop_sound();
        }
        Ok(())
    }

    pub fn is_playing(&self) -> bool {
        let inner = self.lock();
        inner.player.as_ref().is_some_and(|p| p.is_playing())
    }

    /// Open the speaker if it is not open yet, and report the format it
    /// runs at. Sounds are decoded into that format once, here, rather
    /// than on every play.
    fn ensure_player(&self) -> Result<AudioFormat> {
        {
            let inner = self.alive_mut()?;
            if let Some(player) = inner.player.as_ref() {
                return Ok(player.format());
            }
        }

        let request = {
            let inner = self.lock();
            FormatRequest {
                device: inner.config.fixed.output_device.clone(),
                preferred: inner.config.fixed.read_format,
            }
        };
        let stream = self.backend_lock().open_output(&request)?;
        let player = Player::start(stream, Arc::clone(&self.dispatcher))?;
        let format = player.format();

        let mut inner = self.alive_mut()?;
        // Another thread may have opened it while the device was being
        // set up. One owner only, so the first one wins.
        if let Some(existing) = inner.player.as_ref() {
            return Ok(existing.format());
        }
        inner.player = Some(player);
        Ok(format)
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
            if let Some(mut capture) = inner.capture.take() {
                capture.stop();
            }
            if let Some(mut speech) = inner.speech.take() {
                speech.shutdown();
            }
            inner.consumers.clear();
            if let Some(mut player) = inner.player.take() {
                player.shutdown();
            }
            inner.sounds.clear();
        }
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

    /// How much recent audio each consumer keeps. Sets the ceiling on
    /// pre-roll, so it cannot change while capture is running.
    pub fn set_ring_capacity(&self, capacity: Duration) -> Result<()> {
        let mut inner = self.stopped_only("the ring capacity")?;
        let mut candidate = inner.config.clone();
        candidate.fixed.ring_capacity = capacity;
        candidate.validate()?;
        inner.config = candidate;
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

/// The three consumers, each with its own queue, format, and frame
/// size. They are built together but share nothing.
fn build_consumers(config: &Config, wake_wanted: bool, speech_wanted: bool) -> Vec<Consumer> {
    let fixed = &config.fixed;
    let capacity = fixed.ring_capacity;

    vec![
        // The read path is always on: an application that only reads
        // raw audio never has to know the detectors exist.
        Consumer::new(ConsumerKind::Read, fixed.read_format, None, capacity, true),
        Consumer::new(
            ConsumerKind::Wake,
            fixed.wake_format,
            fixed.wake_format.frame_samples(Target::Wake),
            capacity,
            wake_wanted,
        ),
        Consumer::new(
            ConsumerKind::Speech,
            fixed.speech_format,
            fixed.speech_format.frame_samples(Target::Speech),
            capacity,
            speech_wanted,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use backend::fake::FakeBackend;
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
