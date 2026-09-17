//! A local, real-time audio front end: it captures a microphone, spots
//! a wake word, notices when speech has ended, and plays sounds. It
//! understands no speech and never reaches the network.

pub mod backend;
pub mod config;
pub mod error;
pub mod events;

// Machinery, not surface. Crate-only, so "one owner per device" is a
// rule the compiler holds rather than one the documentation asks for.
pub(crate) mod capture;
pub(crate) mod player;
pub(crate) mod speech;
pub(crate) mod wake;

// The few types from those modules that an application does touch.
pub use capture::{AudioChunk, Samples};
pub use player::registry::{SoundId, SoundSource};

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use std::time::Duration;

use backend::{AudioBackend, DeviceInfo, FormatRequest, SupportedFormat};
use capture::{CaptureThread, Consumer, ConsumerKind};
use config::{AudioFormat, Config, Device, Target, TunableConfig};
use error::{Error, Result};
use events::Event;
use events::dispatch::{DEFAULT_QUEUE_CAPACITY, Dispatcher};
use player::Player;
use player::registry::Registry;
use speech::SpeechThread;
use wake::model::WakeModel;
use wake::{Detector as WakeDetector, WakeThread};

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
    player: Option<Arc<Player>>,
    sounds: Arc<Mutex<Registry>>,
    /// Runs while capture runs. Detection happens on its own thread, so
    /// inference never stalls the audio path.
    speech: Option<Arc<SpeechThread>>,
    /// Which detectors the application has asked for. Kept apart from
    /// the consumers because a detector may be switched on before there
    /// is anything to switch, and that intent must survive until start.
    wake_wanted: bool,
    speech_wanted: bool,
    /// Runs while capture runs, once the models have been supplied.
    wake: Option<Arc<WakeThread>>,
    /// Where the three models live. The application supplies all of
    /// them; this library ships no wake word and no way to make one.
    wake_models: Option<(std::path::PathBuf, std::path::PathBuf)>,
    wake_word: Option<std::path::PathBuf>,
    /// Played when the wake word is heard, if the application named one.
    alert: Option<String>,
    /// The settings as they stand, shared rather than copied, so a
    /// change reaches the running threads and not only the next start.
    tunable: Arc<Mutex<TunableConfig>>,
    /// Set while a wake word recording is waiting for its alert to end.
    waiting_now: Arc<AtomicBool>,
    /// What the microphone opened at, which is not always what was
    /// asked for. Only while capture runs.
    opened_input: Option<AudioFormat>,
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

    /// When cpal-backend is disabled but tinypipewire-backend is enabled,
    /// tinypipewire is used as the default backend.
    #[cfg(all(
        not(feature = "cpal-backend"),
        feature = "tinypipewire-backend",
        target_os = "linux"
    ))]
    pub fn new() -> Result<Self> {
        Self::with_tinypipewire()
    }

    /// Without a device backend compiled in there is nothing to open,
    /// so a caller must supply one.
    #[cfg(not(any(
        feature = "cpal-backend",
        all(feature = "tinypipewire-backend", target_os = "linux")
    )))]
    pub fn new() -> Result<Self> {
        Err(Error::NoDevice(config::Device::Input))
    }

    /// Build a handle explicitly backed by tinypipewire.
    #[cfg(all(feature = "tinypipewire-backend", target_os = "linux"))]
    pub fn with_tinypipewire() -> Result<Self> {
        Self::with_backend(Box::new(
            backend::tinypipewire_backend::TinypipewireBackend::new()?,
        ))
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
                sounds: Arc::new(Mutex::new(Registry::new())),
                speech: None,
                wake_wanted: false,
                speech_wanted: false,
                wake: None,
                wake_models: None,
                wake_word: None,
                alert: None,
                tunable: Arc::new(Mutex::new(TunableConfig::default())),
                waiting_now: Arc::new(AtomicBool::new(false)),
                opened_input: None,
            }),
            dispatcher: Arc::new(Dispatcher::new(DEFAULT_QUEUE_CAPACITY)),
            backend: Mutex::new(backend),
        })
    }

    // ── lifecycle ────────────────────────────────────────────────────

    /// Open the microphone and begin reading it. Formats and devices
    /// are fixed from here until the handle is stopped.
    pub fn start(&self) -> Result<()> {
        let mut inner = self.lock();
        match inner.state {
            HandleState::Destroyed => return Err(Error::Destroyed),
            HandleState::Running => return Err(Error::AlreadyRunning),
            HandleState::Created => {}
        }
        note_failure("the settings cannot be used", inner.config.validate())?;

        let request = FormatRequest {
            device: inner.config.fixed.input_device.clone(),
            wanted: inner.config.fixed.input_device_format,
        };
        log::debug!(
            "opening the microphone: device {:?}, format {:?}",
            request.device,
            request.wanted
        );
        let stream = note_failure(
            "the microphone would not open",
            self.backend_lock().open_input(&request),
        )?;
        log::info!("microphone opened at {:?}", stream.format());
        inner.opened_input = Some(stream.format());

        let consumers = build_consumers(&inner.config, inner.wake_wanted, inner.speech_wanted);
        let capture = note_failure(
            "the capture thread would not start",
            CaptureThread::start(stream, consumers.clone(), Arc::clone(&self.dispatcher)),
        )?;

        let speech_ring = consumers
            .iter()
            .find(|c| c.kind == ConsumerKind::Speech)
            .map(|c| Arc::clone(&c.ring))
            .expect("a speech consumer is always built");
        let speech = Arc::new(note_failure(
            "the speech thread would not start",
            SpeechThread::start(
                speech_ring,
                inner.config.fixed.speech_format,
                Arc::clone(&inner.tunable),
                Arc::clone(&self.dispatcher),
            ),
        )?);

        // Here rather than in the application's handler, so a slow one
        // cannot let the tail of an alert count as speech. Every sound
        // also closes off the history behind it.
        if let Some(player) = inner.player.as_ref() {
            let speech = Arc::clone(&speech);
            let alert = inner.alert.clone();
            let waiting_now = Arc::clone(&inner.waiting_now);
            player.on_finished(move |id| {
                if alert.as_deref() == Some(id) {
                    alert_ended(&speech, &waiting_now);
                }
            });
        }

        let wake = match (&inner.wake_models, &inner.wake_word) {
            (Some((spectrogram, features)), Some(word)) => {
                let mut model = note_failure(
                    "the wake word feature models would not load",
                    WakeModel::new(spectrogram, features),
                )?;
                note_failure("the wake word would not load", model.load_word(word))?;
                let detector = WakeDetector::new(
                    model,
                    inner.config.tunable.wake_threshold,
                    inner.config.tunable.wake_settle_frames,
                );
                let ring = consumers
                    .iter()
                    .find(|c| c.kind == ConsumerKind::Wake)
                    .map(|c| Arc::clone(&c.ring))
                    .expect("a wake consumer is always built");

                // What happens the moment the wake word is heard.
                let on_wake = {
                    let speech = Arc::clone(&speech);
                    let player = inner.player.clone();
                    let alert = inner.alert.clone();
                    let sounds = Arc::clone(&inner.sounds);
                    let waiting_now = Arc::clone(&inner.waiting_now);
                    let tunable = Arc::clone(&inner.tunable);
                    Box::new(move || {
                        let (pre_roll, waits) = {
                            let t = tunable.lock().unwrap_or_else(|e| e.into_inner());
                            (t.pre_roll, t.wake_recording_waits_for_alert)
                        };
                        match (&player, &alert) {
                            // With an alert, the recording collects
                            // audio at once but does not count silence
                            // until the sound has finished.
                            (Some(player), Some(alert)) => {
                                let found = sounds
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .get(alert)
                                    .cloned();
                                match found {
                                    Ok(sound) => {
                                        player.play(sound, false);
                                        if waits {
                                            waiting_now.store(true, Ordering::Relaxed);
                                            speech.open_recording(Duration::ZERO, false);
                                        } else {
                                            speech.open_recording(pre_roll, false);
                                        }
                                    }
                                    // The alert was released since it
                                    // was named. Nothing to wait for.
                                    Err(_) => {
                                        log::warn!(
                                            "the alert {alert:?} is no longer registered, \
                                             so the recording opens without it"
                                        );
                                        speech.open_recording(pre_roll, true)
                                    }
                                }
                            }
                            // Without one there is nothing to wait for.
                            _ => speech.open_recording(pre_roll, true),
                        }
                    }) as wake::OnWake
                };

                Some(Arc::new(note_failure(
                    "the wake word thread would not start",
                    WakeThread::start(detector, ring, Arc::clone(&self.dispatcher), on_wake),
                )?))
            }
            _ => None,
        };

        inner.consumers = consumers;
        inner.capture = Some(capture);
        inner.speech = Some(speech);
        log::info!(
            "capture started: wake word {}, speech {}",
            if inner.wake_wanted { "on" } else { "off" },
            if inner.speech_wanted { "on" } else { "off" }
        );
        inner.wake = wake;
        inner.state = HandleState::Running;
        Ok(())
    }

    /// Release the microphone and the speaker. The handle can be
    /// configured and started again afterwards.
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
                if let Some(speech) = inner.speech.take() {
                    speech.shutdown();
                }
                if let Some(wake) = inner.wake.take() {
                    wake.shutdown();
                }
                inner.consumers.clear();
                inner.opened_input = None;
                log::info!("capture stopped");
                Ok(())
            }
        }
    }

    // ── reading live audio ───────────────────────────────────────────

    /// Take the next block of live audio, whether or not a detector is
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

    // ── wake word ────────────────────────────────────────────────────

    /// Supply the two models every wake word shares. Neither knows any
    /// word, and neither is shipped here, because their terms are not
    /// the ones this library is offered under.
    pub fn load_wake_features(&self, spectrogram: &Path, features: &Path) -> Result<()> {
        let mut inner = self.stopped_only("the wake word models")?;
        // Checked now rather than at the next start, so a wrong path is
        // reported where it was given.
        WakeModel::new(spectrogram, features)?;
        log::info!(
            "wake word feature models loaded from {} and {}",
            spectrogram.display(),
            features.display()
        );
        inner.wake_models = Some((spectrogram.to_path_buf(), features.to_path_buf()));
        Ok(())
    }

    /// Supply the model for the phrase to listen for. Its shape is
    /// checked here; names inside it are not, since every model has its
    /// own and any of them works.
    pub fn load_wake_model(&self, path: &Path) -> Result<()> {
        let mut inner = self.stopped_only("the wake word model")?;
        let (spectrogram, features) = inner.wake_models.clone().ok_or(Error::NoWakeModel)?;
        let mut model = WakeModel::new(&spectrogram, &features)?;
        model.load_word(path)?;
        log::info!("wake word model loaded from {}", path.display());
        inner.wake_word = Some(path.to_path_buf());
        Ok(())
    }

    /// Start listening for the wake word. Naming a sound plays it on
    /// detection and holds off counting silence until it ends, so its
    /// tail is not taken for speech.
    pub fn enable_wake(&self, alert: Option<&str>) -> Result<()> {
        {
            let mut inner = self.alive_mut()?;
            if inner.wake_word.is_none() {
                return Err(Error::NoWakeModel);
            }
            if let Some(alert) = alert {
                // Named now so a missing sound is reported here, not
                // silently when the wake word is finally heard.
                inner
                    .sounds
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(alert)?;
                inner.alert = Some(alert.to_string());
            } else {
                inner.alert = None;
            }
        }
        self.set_consumer(ConsumerKind::Wake, true)
    }

    /// The sound played when the wake word is heard, if any.
    pub fn wake_alert(&self) -> Option<String> {
        self.lock().alert.clone()
    }

    /// How sure the detector was, most recently: every score, because
    /// setting a threshold is guesswork without seeing the near misses.
    pub fn wake_score(&self) -> Option<f32> {
        self.lock().wake.as_ref().and_then(|w| w.last_score())
    }

    /// Stop listening for the wake word. The models stay loaded.
    pub fn disable_wake(&self) -> Result<()> {
        self.set_consumer(ConsumerKind::Wake, false)
    }

    /// True while the wake word is being listened for.
    pub fn is_wake_enabled(&self) -> bool {
        self.lock().wake_wanted
    }

    /// Forget what has been heard, so a fresh utterance is needed.
    pub fn reset_wake(&self) -> Result<()> {
        let inner = self.alive_mut()?;
        if let Some(wake) = inner.wake.as_ref() {
            wake.reset();
        }
        Ok(())
    }

    // ── speech detection ─────────────────────────────────────────────

    /// Start listening for the end of speech. Works before or after
    /// capture starts, taking effect on the next block of audio.
    pub fn enable_speech(&self) -> Result<()> {
        self.set_consumer(ConsumerKind::Speech, true)
    }

    /// Stop watching for speech. Any open recording is dropped.
    pub fn disable_speech(&self) -> Result<()> {
        self.set_consumer(ConsumerKind::Speech, false)
    }

    /// True while speech and silence are being watched for.
    pub fn is_speech_enabled(&self) -> bool {
        self.lock().speech_wanted
    }

    /// Open a recording now, without waiting for a wake word.
    ///
    /// One `SpeechEnded` follows, carrying the audio and why it ended.
    pub fn start_recording(&self) -> Result<()> {
        let inner = self.running_only()?;
        let speech = inner.speech.as_ref().ok_or(Error::NotRunning)?;
        // Pre-roll reaches back into audio already gone by, and is off
        // by default because most callers want only what follows.
        speech.open_recording(inner.config.tunable.pre_roll, true);
        log::debug!("recording opened by the application");
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
        log::debug!("recording stopped by the application");
        Ok(())
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
        log::debug!("{kind:?} consumer turned {}", if on { "on" } else { "off" });
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

    /// Register a sound so it can be played later. Allowed while
    /// running, because it adds an asset and rebuilds no pipeline, so a
    /// reply arriving at run time can be played.
    pub fn register_sound(&self, id: &str, source: SoundSource, volume: f32) -> Result<()> {
        let output = self.ensure_player()?;
        let inner = self.alive_mut()?;
        inner
            .sounds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .register(id.to_string(), source, volume, output)?;
        log::debug!("sound {id:?} registered at volume {volume}");
        Ok(())
    }

    /// Forget a sound and release the audio it held.
    pub fn unregister_sound(&self, id: &str) -> Result<()> {
        let inner = self.alive_mut()?;
        let mut sounds = inner.sounds.lock().unwrap_or_else(|e| e.into_inner());
        sounds.unregister(id)
    }

    /// Play a registered sound, optionally repeating until stopped.
    /// A newer sound supersedes whatever was playing.
    pub fn play_sound(&self, id: &str, repeat: bool) -> Result<()> {
        self.ensure_player()?;
        let inner = self.alive_mut()?;
        let sound = inner
            .sounds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)?
            .clone();
        let player = inner
            .player
            .as_ref()
            .ok_or(Error::NoDevice(config::Device::Output))?;
        player.play(sound, repeat);
        log::debug!("playing sound {id:?}, repeat {repeat}");
        Ok(())
    }

    /// Cut playback short. No completion event follows, because the
    /// sound did not end on its own.
    pub fn stop_sound(&self) -> Result<()> {
        let inner = self.alive_mut()?;
        let cut = inner.player.as_ref().and_then(|p| p.stop_sound());
        // No completion event reports a sound that was cut, so the
        // recording behind an alert would wait for one that never came.
        if cut.is_some()
            && cut == inner.alert
            && let Some(speech) = inner.speech.as_ref()
        {
            alert_ended(speech, &inner.waiting_now);
        }
        Ok(())
    }

    /// True while a sound is coming out of the speaker.
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
                wanted: inner.config.fixed.output_device_format,
            }
        };
        log::debug!(
            "opening the speaker: device {:?}, format {:?}",
            request.device,
            request.wanted
        );
        let stream = note_failure(
            "the speaker would not open",
            self.backend_lock().open_output(&request),
        )?;
        let player = Arc::new(note_failure(
            "the playback thread would not start",
            Player::start(stream, Arc::clone(&self.dispatcher)),
        )?);
        let format = player.format();

        let mut inner = self.alive_mut()?;
        // Another thread may have opened it while the device was being
        // set up. One owner only, so the first one wins.
        if let Some(existing) = inner.player.as_ref() {
            return Ok(existing.format());
        }
        inner.player = Some(player);
        log::info!("speaker opened at {format:?}");
        Ok(format)
    }

    /// True between a successful start and a stop.
    pub fn is_running(&self) -> bool {
        self.lock().state == HandleState::Running
    }

    /// Where the handle is in its life: made, running, or destroyed.
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
            if let Some(speech) = inner.speech.take() {
                speech.shutdown();
            }
            if let Some(wake) = inner.wake.take() {
                wake.shutdown();
            }
            inner.consumers.clear();
            if let Some(player) = inner.player.take() {
                player.shutdown();
            }
            inner
                .sounds
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }
        self.dispatcher.shutdown();
        log::info!("handle destroyed");
    }

    // ── events ───────────────────────────────────────────────────────

    /// Set the handler for every notification. It runs on the
    /// dispatcher thread, so a slow one delays later events only.
    pub fn on_event(&self, handler: impl Fn(Event) + Send + Sync + 'static) -> Result<()> {
        self.alive()?;
        self.dispatcher.set_handler(Box::new(handler));
        Ok(())
    }

    // ── configuration ────────────────────────────────────────────────

    /// Set the audio format one consumer receives. Only before capture
    /// starts, because once it runs the conversion pipeline is built
    /// and changing this would mean rebuilding it.
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

    /// Choose a microphone by its identifier. `None` means the one
    /// the system prefers. Fixed once capture has started.
    pub fn set_input_device(&self, name: Option<&str>) -> Result<()> {
        let mut inner = self.stopped_only("the input device")?;
        inner.config.fixed.input_device = name.map(str::to_string);
        Ok(())
    }

    /// Choose a speaker by its identifier. `None` means the one the
    /// system prefers. Fixed once capture has started.
    pub fn set_output_device(&self, name: Option<&str>) -> Result<()> {
        let mut inner = self.speaker_shut_only("the output device")?;
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
        // Every tunable is set through here, so this is the one place
        // the shared copy has to be kept in step.
        *inner.tunable.lock().unwrap_or_else(|e| e.into_inner()) = inner.config.tunable.clone();
        log::debug!("settings now {:?}", inner.config.tunable);
        Ok(())
    }

    /// How long the detector looks away after hearing the wake word,
    /// in frames of 80 ms. Long enough that what was just heard is not
    /// heard again; longer is time spent unable to hear more.
    pub fn set_wake_settle_frames(&self, frames: u32) -> Result<()> {
        self.tune(|c| c.tunable.wake_settle_frames = frames)
    }

    /// How sure the detector must be before it says it heard the
    /// word, from 0.0 to 1.0. Changeable while running.
    pub fn set_wake_threshold(&self, value: f32) -> Result<()> {
        self.tune(|c| c.tunable.wake_threshold = value)?;
        let inner = self.lock();
        if let Some(wake) = inner.wake.as_ref() {
            wake.set_threshold(value);
        }
        Ok(())
    }

    /// How readily audio counts as speech. These next four are weighed
    /// frame by frame, so a new value reaches an open recording too.
    pub fn set_speech_threshold(&self, value: f32) -> Result<()> {
        self.tune(|c| c.tunable.speech_threshold = value)
    }

    /// How long the speaker must be quiet before a recording ends.
    /// Shortened below the quiet already gathered, it ends at once.
    pub fn set_silence_duration(&self, value: std::time::Duration) -> Result<()> {
        self.tune(|c| c.tunable.silence_duration = value)
    }

    /// The longest a recording may run, whatever the speaker is doing.
    /// Shortened below what an open one has run, it is handed over now.
    pub fn set_max_recording(&self, value: std::time::Duration) -> Result<()> {
        self.tune(|c| c.tunable.max_recording = value)
    }

    /// How long to wait for anyone to speak at all before giving up
    /// on a recording.
    pub fn set_no_speech_timeout(&self, value: std::time::Duration) -> Result<()> {
        self.tune(|c| c.tunable.no_speech_timeout = value)
    }

    /// How much audio from before the recording opened to include, so
    /// a word begun early is not cut off. Limited by the history, and
    /// used by a recording the wake word opened as much as by one the
    /// application asked for.
    pub fn set_pre_roll(&self, value: std::time::Duration) -> Result<()> {
        if value > Duration::ZERO && self.lock().config.tunable.wake_recording_waits_for_alert {
            log::warn!(
                "a wake word recording waits for the alert, so this pre-roll \
                 reaches only recordings the application opens itself"
            );
        }
        self.tune_recording("the pre-roll", |c| c.tunable.pre_roll = value)
    }

    /// Whether a recording the wake word opened begins again when the
    /// alert ends, leaving behind what was heard while it played.
    ///
    /// Off by default: audio is collected from the moment the wake word
    /// lands, so a speaker who talks over the alert is kept, at the
    /// price of the alert itself being in the recording wherever the
    /// microphone can hear the speaker. Turning it on makes the
    /// pre-roll unreachable on that path, because all it could reach
    /// back into is the alert. A recording `start_recording` opened is
    /// not affected either way.
    pub fn set_wake_recording_waits_for_alert(&self, value: bool) -> Result<()> {
        if value && self.lock().config.tunable.pre_roll > Duration::ZERO {
            log::warn!(
                "the pre-roll is set, but a wake word recording now waits for \
                 the alert, so nothing from before the alert reaches it"
            );
        }
        self.tune_recording("waiting for the alert", |c| {
            c.tunable.wake_recording_waits_for_alert = value
        })
    }

    /// True while a recording is collecting audio.
    pub fn is_recording(&self) -> bool {
        self.lock()
            .speech
            .as_ref()
            .is_some_and(|s| s.is_recording())
    }

    /// Change a setting read only as a recording opens. Refused while
    /// one is open, because a new value could not reach it.
    fn tune_recording(&self, what: &'static str, apply: impl FnOnce(&mut Config)) -> Result<()> {
        if self.is_recording() {
            return Err(Error::RecordingOpen { what });
        }
        self.tune(apply)
    }

    /// A copy of every setting as it stands now.
    pub fn config(&self) -> Config {
        self.lock().config.clone()
    }

    // ── devices ──────────────────────────────────────────────────────

    /// Every microphone the system offers.
    pub fn input_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.alive()?;
        self.backend_lock().input_devices()
    }

    /// Every speaker the system offers.
    pub fn output_devices(&self) -> Result<Vec<DeviceInfo>> {
        self.alive()?;
        self.backend_lock().output_devices()
    }

    /// Open the microphone at this, rather than at whatever it offers
    /// by default. `None` goes back to the default.
    ///
    /// Refused here if the named device does not offer it, and again
    /// when capture starts, because the device may have changed.
    pub fn set_input_device_format(&self, format: Option<AudioFormat>) -> Result<()> {
        let mut inner = self.stopped_only("the microphone format")?;
        if let Some(wanted) = format {
            let device = inner.config.fixed.input_device.clone();
            let offered = self.backend_lock().input_formats(device.as_deref())?;
            refuse_unless_offered(&offered, wanted, Device::Input)?;
        }
        inner.config.fixed.input_device_format = format;
        Ok(())
    }

    /// Open the speaker at this. Fixed once the speaker is open, which
    /// is when the first sound is registered or played.
    pub fn set_output_device_format(&self, format: Option<AudioFormat>) -> Result<()> {
        let mut inner = self.speaker_shut_only("the speaker format")?;
        if let Some(wanted) = format {
            let device = inner.config.fixed.output_device.clone();
            let offered = self.backend_lock().output_formats(device.as_deref())?;
            refuse_unless_offered(&offered, wanted, Device::Output)?;
        }
        inner.config.fixed.output_device_format = format;
        Ok(())
    }

    /// What the microphone opened at, which is not always what was
    /// asked for. `None` until capture starts.
    pub fn input_format(&self) -> Option<AudioFormat> {
        self.lock().opened_input
    }

    /// What the speaker opened at. `None` until the first sound opens
    /// it.
    pub fn output_format(&self) -> Option<AudioFormat> {
        self.lock().player.as_ref().map(|p| p.format())
    }

    /// What a microphone will take, by identifier or `None` for the
    /// default. Rates come as ranges, which is how a device says it.
    pub fn input_device_formats(&self, device: Option<&str>) -> Result<Vec<SupportedFormat>> {
        self.alive()?;
        self.backend_lock().input_formats(device)
    }

    /// What a speaker will take. Sounds are converted to whichever of
    /// these the speaker is opened at, so this says what to expect.
    pub fn output_device_formats(&self, device: Option<&str>) -> Result<Vec<SupportedFormat>> {
        self.alive()?;
        self.backend_lock().output_formats(device)
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

    /// The speaker opens on the first sound and stays open until the
    /// handle is destroyed, so capture's state says nothing about it.
    fn speaker_shut_only(&self, what: &'static str) -> Result<MutexGuard<'_, Inner>> {
        let inner = self.alive_mut()?;
        if inner.player.is_some() {
            return Err(Error::RunningNotAllowed { what });
        }
        Ok(inner)
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

/// A format the device never offered is refused by name of what it
/// does, rather than quietly turning into something else.
pub(crate) fn refuse_unless_offered(
    offered: &[SupportedFormat],
    wanted: AudioFormat,
    device: Device,
) -> Result<()> {
    if offered.iter().any(|f| f.covers(&wanted)) {
        return Ok(());
    }
    Err(Error::DeviceFormat {
        device,
        got: format!(
            "{} Hz, {} channels, {}",
            wanted.sample_rate, wanted.channels, wanted.sample_type
        ),
        offered: describe_offered(offered),
    })
}

/// What a device offers, short enough to put in a message.
pub(crate) fn describe_offered(offered: &[SupportedFormat]) -> String {
    if offered.is_empty() {
        return "nothing this library can speak".to_string();
    }
    offered
        .iter()
        .map(|f| {
            let rate = if f.min_sample_rate == f.max_sample_rate {
                f.min_sample_rate.to_string()
            } else {
                format!("{}-{}", f.min_sample_rate, f.max_sample_rate)
            };
            format!("{rate} Hz/{}ch/{}", f.channels, f.sample_type)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Wait for a worker and say so if it panicked. Nothing else reports
/// it: the thread simply stops, and the audio stops with it.
pub(crate) fn join_worker(worker: JoinHandle<()>, what: &str) {
    if worker.join().is_err() {
        log::error!("the {what} thread panicked");
    }
}

/// Say why something failed on the way up. The caller is told as well,
/// but the log would otherwise stop at the attempt.
fn note_failure<T>(what: &str, result: Result<T>) -> Result<T> {
    if let Err(e) = &result {
        log::warn!("{what}: {e}");
    }
    result
}

/// What the end of an alert means to the recording behind it, whether
/// it played out or was cut short.
fn alert_ended(speech: &SpeechThread, waiting: &AtomicBool) {
    if waiting.swap(false, Ordering::Relaxed) {
        // Opening again leaves behind whatever the microphone heard of
        // the alert.
        speech.open_recording(Duration::ZERO, true);
    } else {
        speech.start_counting();
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
