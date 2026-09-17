pub mod envelope;
pub mod registry;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use crate::backend::OutputStream;
use crate::capture::Samples;
use crate::config::AudioFormat;
use crate::error::{Error, Result};
use crate::events::Event;
use crate::events::dispatch::Dispatcher;
use crate::player::registry::RegisteredSound;

/// Samples handed to the device at a time.
const WRITE_CHUNK: usize = 512;

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

/// The one owner of the speaker. Alerts, waiting loops, and spoken
/// replies all go through here, because two owners would put two claims
/// on one device.
pub struct Player {
    state: Arc<Mutex<State>>,
    wake: Arc<Condvar>,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
    format: AudioFormat,
    /// Told when a sound ends on its own, before the application hears
    /// about it. This is how the alert gate is released.
    on_finished: Finished,
}

/// Told when a sound ends on its own.
type Finished = Arc<Mutex<Option<Box<dyn Fn(&str) + Send + Sync>>>>;

impl Player {
    pub fn start(stream: Box<dyn OutputStream>, dispatcher: Arc<Dispatcher>) -> Result<Self> {
        let format = stream.format();
        let state = Arc::new(Mutex::new(State::default()));
        let wake = Arc::new(Condvar::new());
        let stop = Arc::new(AtomicBool::new(false));
        let finished: Finished = Arc::new(Mutex::new(None));

        let worker = {
            let state = Arc::clone(&state);
            let wake = Arc::clone(&wake);
            let stop = Arc::clone(&stop);
            let hooks = Arc::clone(&finished);
            thread::Builder::new()
                .name("edge-ear-player".to_string())
                .spawn(move || run(stream, state, wake, stop, dispatcher, hooks))
                .map_err(|e| Error::Backend {
                    device: crate::config::Device::Output,
                    reason: format!("player thread would not start: {e}"),
                })?
        };

        Ok(Self {
            state,
            wake,
            stop,
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
        self.wake.notify_all();
    }

    /// Cut playback short, naming what was cut. No completion event
    /// follows, because the sound did not finish on its own.
    pub fn stop_sound(&self) -> Option<String> {
        let mut state = self.lock();
        let cut = state.current.take().map(|p| p.sound.id.clone());
        state.stopped_by_application = true;
        drop(state);
        self.wake.notify_all();
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
            self.stop.store(true, Ordering::Relaxed);
        }
        self.wake.notify_all();
        let worker = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(worker) = worker {
            crate::join_worker(worker, "player");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(
    mut stream: Box<dyn OutputStream>,
    state: Arc<Mutex<State>>,
    wake: Arc<Condvar>,
    stop: Arc<AtomicBool>,
    dispatcher: Arc<Dispatcher>,
    hooks: Finished,
) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Take the next block, and note whether that block finished the
        // sound. The lock is released before writing, so play and stop
        // stay responsive while the device is being fed.
        let next = {
            let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if stop.load(Ordering::Relaxed) {
                    return;
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
                    let mut ended_id = None;
                    if finished {
                        if playing.repeat {
                            playing.position = 0;
                        } else {
                            ended_id = Some(playing.sound.id.clone());
                            guard.current = None;
                        }
                    }
                    break Some((block, ended_id));
                }
                guard = wake
                    .wait(guard)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };

        let Some((block, ended_id)) = next else { break };

        if !block.is_empty() {
            let block = Samples::I16(block);
            // The device may have no room yet. Keep offering it, but
            // check between attempts whether we have been told to stop.
            loop {
                match stream.write(&block) {
                    Ok(()) => break,
                    Err(Error::Timeout) => {
                        if stop.load(Ordering::Relaxed) {
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
        }

        // Only a sound that ran to its own end is reported. A sound the
        // application stopped never gets here.
        if let Some(id) = ended_id {
            log::debug!("sound {id:?} finished");
            if let Some(hook) = hooks.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                hook(&id);
            }
            dispatcher.emit(Event::SoundFinished { id });
        }
    }

    let _ = stream.stop();
}
