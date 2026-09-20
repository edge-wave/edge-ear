pub mod envelope;
pub mod registry;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::backend::OutputStream;
use crate::capture::Samples;
use crate::config::{AudioFormat, Device};
use crate::error::{Error, Result};
use crate::events::Event;
use crate::events::dispatch::Dispatcher;
use crate::player::registry::RegisteredSound;

/// Samples handed to the device at a time.
const WRITE_CHUNK: usize = 512;

/// How long the speaker may sit idle before it is let go. Long enough
/// to cover one sound following another, short enough to free the device.
const OUTPUT_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

/// Opens the speaker again after it was let go. The player owns no
/// backend, so this is how it reaches one.
pub(crate) type OpenOutput = Box<dyn Fn() -> Result<Box<dyn OutputStream>> + Send>;

/// What is playing right now, if anything.
struct Playing {
    sound: RegisteredSound,
    position: usize,
    repeat: bool,
}

#[derive(Default)]
struct State {
    current: Option<Playing>,
    /// Raised by the application, so a natural end can be told apart
    /// from being cut short. Only a natural end is reported.
    stopped_by_application: bool,
}

/// What the player and its worker both hold.
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    stop: AtomicBool,
}

/// What the worker knows about the device it is feeding.
struct Speaker {
    /// `None` between an idle close and the next sound.
    stream: Option<Box<dyn OutputStream>>,
    open: OpenOutput,
    /// What sounds were decoded into. A device that comes back at
    /// anything else cannot play them.
    format: AudioFormat,
    idle: Duration,
}

/// Why the worker stopped waiting.
enum Next {
    /// Audio to hand over, and the sound it finished, if it did.
    Play {
        block: Vec<i16>,
        ended: Option<String>,
    },
    /// Nothing has played for long enough that the device should go.
    Close,
    Stop,
}

/// The one owner of the speaker. Alerts, waiting loops, and spoken
/// replies all go through here, because two owners would put two claims
/// on one device.
pub struct Player {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
    format: AudioFormat,
    /// Told when a sound ends on its own, before the application hears
    /// about it. This is how the alert gate is released.
    on_finished: Finished,
}

/// Told when a sound ends on its own.
type Finished = Arc<Mutex<Option<Box<dyn Fn(&str) + Send + Sync>>>>;

impl Player {
    pub fn start(
        stream: Box<dyn OutputStream>,
        open: OpenOutput,
        dispatcher: Arc<Dispatcher>,
    ) -> Result<Self> {
        Self::start_with_idle(stream, open, dispatcher, OUTPUT_IDLE_TIMEOUT)
    }

    /// The idle timeout is an argument only so tests need not sit
    /// through it. Every caller outside them takes the constant.
    fn start_with_idle(
        stream: Box<dyn OutputStream>,
        open: OpenOutput,
        dispatcher: Arc<Dispatcher>,
        idle: Duration,
    ) -> Result<Self> {
        let format = stream.format();
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let finished: Finished = Arc::new(Mutex::new(None));

        let worker = {
            let shared = Arc::clone(&shared);
            let hooks = Arc::clone(&finished);
            let speaker = Speaker {
                stream: Some(stream),
                open,
                format,
                idle,
            };
            thread::Builder::new()
                .name("edge-ear-player".to_string())
                .spawn(move || run(speaker, shared, dispatcher, hooks))
                .map_err(|e| Error::Backend {
                    device: Device::Output,
                    reason: format!("player thread would not start: {e}"),
                })?
        };

        Ok(Self {
            shared,
            worker: Mutex::new(Some(worker)),
            format,
            on_finished: finished,
        })
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Start a sound, replacing whatever was playing.
    pub fn play(&self, sound: RegisteredSound, repeat: bool) {
        let mut state = self.lock();
        state.current = Some(Playing {
            sound,
            position: 0,
            repeat,
        });
        state.stopped_by_application = false;
        drop(state);
        self.shared.wake.notify_all();
    }

    /// Cut playback short, naming what was cut. No completion event
    /// follows, because the sound did not finish on its own.
    pub fn stop_sound(&self) -> Option<String> {
        let mut state = self.lock();
        let cut = state.current.take().map(|p| p.sound.id.clone());
        state.stopped_by_application = true;
        drop(state);
        self.shared.wake.notify_all();
        cut
    }

    pub fn is_playing(&self) -> bool {
        self.lock().current.is_some()
    }

    /// Told when a sound ends on its own. Replaces any earlier one.
    pub fn on_finished(&self, hook: impl Fn(&str) + Send + Sync + 'static) {
        *self.on_finished.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(hook));
    }

    pub fn shutdown(&self) {
        // Set under the lock the worker parks beneath, because a stop
        // reaching it between its look and its wait would be lost.
        {
            let _state = self.lock();
            self.shared.stop.store(true, Ordering::Relaxed);
        }
        self.shared.wake.notify_all();
        let worker = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(worker) = worker {
            crate::join_worker(worker, "player");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Park until there is audio, the idle deadline passes, or the
    /// player stops. With no device open there is nothing to wake for.
    fn wait_for_work(&self, deadline: Instant, open: bool) -> Next {
        let mut guard = self.lock();
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Next::Stop;
            }
            if let Some(playing) = guard.current.as_mut() {
                let samples = &playing.sound.samples;
                let end = (playing.position + WRITE_CHUNK).min(samples.len());
                let block: Vec<i16> = samples[playing.position..end]
                    .iter()
                    .map(|s| (*s as f32 * playing.sound.volume) as i16)
                    .collect();
                playing.position = end;

                let finished = playing.position >= samples.len();
                let mut ended = None;
                if finished {
                    if playing.repeat {
                        playing.position = 0;
                    } else {
                        ended = Some(playing.sound.id.clone());
                        guard.current = None;
                    }
                }
                return Next::Play { block, ended };
            }
            if !open {
                guard = self
                    .wake
                    .wait(guard)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                continue;
            }
            // What is checked is the deadline, not whether the timeout
            // fired, so a spurious wake does not start the wait over.
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Next::Close;
            }
            let (next, _) = self
                .wake
                .wait_timeout(guard, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard = next;
        }
    }

    /// Drop whatever was queued, naming it. Used when the device could
    /// not be had, so the same fault is not reported once per block.
    fn abandon_current(&self) -> Option<String> {
        self.lock().current.take().map(|p| p.sound.id)
    }
}

impl Speaker {
    /// Open the speaker again. Sounds were decoded into the first
    /// format it reported, so another one is no use for them.
    fn reopen(&mut self, dispatcher: &Dispatcher) -> bool {
        let message = match (self.open)() {
            Ok(stream) if stream.format() == self.format => {
                log::debug!("the speaker was opened again at {:?}", self.format);
                self.stream = Some(stream);
                return true;
            }
            Ok(other) => format!(
                "the speaker came back at {:?}, not the {:?} its sounds were prepared for",
                other.format(),
                self.format
            ),
            Err(e) => format!("the speaker would not open again: {e}"),
        };
        log::error!("{message}");
        dispatcher.emit(Event::DeviceError {
            device: Device::Output,
            message,
        });
        false
    }

    /// Let the device go. Draining it takes tens of milliseconds, so
    /// this is called with no lock held.
    fn release(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            if let Err(e) = stream.stop() {
                log::warn!("the idle speaker would not close cleanly: {e}");
            }
            log::debug!("the speaker sat idle, so the device was let go");
        }
    }
}

fn run(mut speaker: Speaker, shared: Arc<Shared>, dispatcher: Arc<Dispatcher>, hooks: Finished) {
    let mut deadline = Instant::now() + speaker.idle;

    loop {
        let (block, ended) = match shared.wait_for_work(deadline, speaker.stream.is_some()) {
            Next::Stop => break,
            Next::Close => {
                speaker.release();
                continue;
            }
            Next::Play { block, ended } => (block, ended),
        };

        // The device is opened here rather than when the sound was
        // asked for, so the application is never held up by it.
        if speaker.stream.is_none() && !speaker.reopen(&dispatcher) {
            // Drop the sound, but still tell whoever waited on it, or
            // a recording held open behind an alert would never close.
            if let Some(id) = shared.abandon_current().or(ended)
                && let Some(hook) = hooks.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
            {
                hook(&id);
            }
            continue;
        }

        if !block.is_empty() {
            let block = Samples::I16(block);
            let stream = speaker
                .stream
                .as_mut()
                .expect("a reopen that did not fail leaves a stream");
            // The device may have no room yet. Keep offering it, but
            // check between attempts whether we have been told to stop.
            loop {
                match stream.write(&block) {
                    Ok(()) => break,
                    Err(Error::Timeout) => {
                        if shared.stop.load(Ordering::Relaxed) {
                            let _ = stream.stop();
                            return;
                        }
                    }
                    Err(e) => {
                        log::error!("the speaker stopped taking audio, so playback ended: {e}");
                        let _ = stream.stop();
                        return;
                    }
                }
            }
            // The idle clock runs from the last audio handed over.
            deadline = Instant::now() + speaker.idle;
        }

        // Only a sound that ran to its own end is reported. A sound the
        // application stopped never gets here.
        if let Some(id) = ended {
            log::debug!("sound {id:?} finished");
            if let Some(hook) = hooks.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                hook(&id);
            }
            dispatcher.emit(Event::SoundFinished { id });
        }
    }

    speaker.release();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::{FakeBackend, PlaybackLog};
    use crate::backend::{AudioBackend, FormatRequest};

    /// Long enough for a worker thread to get there, short enough that
    /// a broken one fails the run rather than hanging it.
    fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        false
    }

    fn sound(id: &str, samples: usize) -> RegisteredSound {
        RegisteredSound {
            id: id.to_string(),
            samples: vec![1_000; samples],
            format: AudioFormat::mono_16k(),
            volume: 1.0,
        }
    }

    struct Rig {
        player: Player,
        log: Arc<Mutex<PlaybackLog>>,
        events: Arc<Mutex<Vec<Event>>>,
    }

    impl Rig {
        fn counts(&self) -> (usize, usize) {
            let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
            (log.opens, log.closes)
        }

        fn written(&self) -> usize {
            self.log
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .written
                .len()
        }
    }

    /// A player over a fake speaker. `refuse` is raised to make every
    /// later open fail, which is how a device lost while idle looks.
    fn rig(idle: Duration, refuse: Arc<AtomicBool>) -> Rig {
        let backend = Arc::new(Mutex::new(FakeBackend::silent()));
        let log = Arc::clone(&backend.lock().unwrap_or_else(|e| e.into_inner()).playback);
        let request = FormatRequest {
            device: None,
            wanted: Some(AudioFormat::mono_16k()),
        };
        let first = backend
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .open_output(&request)
            .expect("the first open");

        let open: OpenOutput = {
            let backend = Arc::clone(&backend);
            Box::new(move || {
                if refuse.load(Ordering::Relaxed) {
                    return Err(Error::NoDevice(Device::Output));
                }
                backend
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .open_output(&request)
            })
        };

        let dispatcher = Arc::new(Dispatcher::new(64));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        dispatcher.set_handler(Box::new(move |event| {
            sink.lock().unwrap_or_else(|e| e.into_inner()).push(event);
        }));

        Rig {
            player: Player::start_with_idle(first, open, dispatcher, idle)
                .expect("the player thread"),
            log,
            events,
        }
    }

    fn willing(idle: Duration) -> Rig {
        rig(idle, Arc::new(AtomicBool::new(false)))
    }

    #[test]
    fn a_speaker_nothing_is_playing_on_is_let_go() {
        let rig = willing(Duration::from_millis(30));
        assert!(
            wait_until(|| rig.counts().1 == 1),
            "an idle speaker must be released"
        );
        assert_eq!(rig.counts().0, 1, "and not opened again while it is quiet");
    }

    #[test]
    fn a_sound_opens_the_speaker_again() {
        let rig = willing(Duration::from_millis(30));
        assert!(wait_until(|| rig.counts().1 == 1), "released while idle");

        rig.player.play(sound("alert", 400), false);
        assert!(
            wait_until(|| rig.counts().0 == 2),
            "playing must open the speaker again"
        );
        assert!(
            wait_until(|| rig.written() >= 400),
            "and the audio must reach it"
        );
    }

    #[test]
    fn a_sound_following_another_finds_the_speaker_still_open() {
        let rig = willing(Duration::from_secs(30));

        rig.player.play(sound("first", 400), false);
        assert!(wait_until(|| rig.written() >= 400), "the first sound plays");
        rig.player.play(sound("second", 400), false);
        assert!(
            wait_until(|| rig.written() >= 800),
            "and so does the second"
        );

        assert_eq!(
            rig.counts(),
            (1, 0),
            "one open, no close: the device was never let go between them"
        );
    }

    #[test]
    fn a_speaker_that_will_not_come_back_is_reported_and_strands_nobody() {
        let refuse = Arc::new(AtomicBool::new(false));
        let rig = rig(Duration::from_millis(30), Arc::clone(&refuse));
        assert!(wait_until(|| rig.counts().1 == 1), "released while idle");

        let told = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&told);
        rig.player.on_finished(move |id| {
            sink.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(id.to_string())
        });

        refuse.store(true, Ordering::Relaxed);
        rig.player.play(sound("alert", 400), false);

        assert!(
            wait_until(|| rig
                .events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|e| matches!(e, Event::DeviceError { .. }))),
            "a speaker that will not open again must be reported"
        );
        assert!(
            wait_until(|| told
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|id| id == "alert")),
            "and whatever waits on the sound must still be let go"
        );
        assert_eq!(rig.written(), 0, "nothing was played");
        assert!(
            !rig.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|e| matches!(e, Event::SoundFinished { .. })),
            "a sound that never played did not finish"
        );
    }
}
