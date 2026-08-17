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
use crate::player::registry::{RegisteredSound, SoundId};

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

/// The one owner of the speaker.
///
/// Alert sounds, waiting loops, and spoken replies all go through here.
/// Splitting playback across two owners would put two claims on one
/// device, which is the problem already solved on the capture side.
pub struct Player {
    state: Arc<Mutex<State>>,
    wake: Arc<Condvar>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    format: AudioFormat,
}

impl Player {
    pub fn start(stream: Box<dyn OutputStream>, dispatcher: Arc<Dispatcher>) -> Result<Self> {
        let format = stream.format();
        let state = Arc::new(Mutex::new(State::default()));
        let wake = Arc::new(Condvar::new());
        let stop = Arc::new(AtomicBool::new(false));

        let worker = {
            let state = Arc::clone(&state);
            let wake = Arc::clone(&wake);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("edge-ear-player".to_string())
                .spawn(move || run(stream, state, wake, stop, dispatcher))
                .map_err(|e| Error::Backend {
                    device: crate::config::Device::Output,
                    reason: format!("player thread would not start: {e}"),
                })?
        };

        Ok(Self {
            state,
            wake,
            stop,
            worker: Some(worker),
            format,
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

    /// Cut playback short. No completion event follows, because the
    /// sound did not finish on its own.
    pub fn stop_sound(&self) {
        let mut state = self.lock();
        state.current = None;
        state.stopped_by_application = true;
        drop(state);
        self.wake.notify_all();
    }

    pub fn is_playing(&self) -> bool {
        self.lock().current.is_some()
    }

    pub fn playing_id(&self) -> Option<SoundId> {
        self.lock().current.as_ref().map(|p| p.sound.id.clone())
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
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
                    Err(_) => {
                        let _ = stream.stop();
                        return;
                    }
                }
            }
        }

        // Only a sound that ran to its own end is reported. A sound the
        // application stopped never gets here.
        if let Some(id) = ended_id {
            dispatcher.emit(Event::SoundFinished { id });
        }
    }

    let _ = stream.stop();
}
