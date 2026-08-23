pub mod model;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::capture::AudioChunk;
use crate::capture::ring::Ring;
use crate::config::Device;
use crate::error::{Error, Result};
use crate::events::Event;
use crate::events::dispatch::Dispatcher;
use crate::wake::model::WakeModel;

/// Decides when a score counts as hearing the wake word.
///
/// After one detection it ignores what follows for a while. Clearing
/// the score is not enough on its own: the audio that caused the
/// detection is still inside the pipeline and would cause another one
/// immediately.
pub struct Detector {
    model: WakeModel,
    threshold: f32,
    settle_frames: u32,
    settling: u32,
}

impl Detector {
    pub fn new(model: WakeModel, threshold: f32, settle_frames: u32) -> Self {
        Self {
            model,
            threshold,
            settle_frames,
            settling: 0,
        }
    }

    pub fn set_threshold(&mut self, threshold: f32) {
        self.threshold = threshold;
    }

    /// Feed one frame. Gives the score when it counts as a detection.
    pub fn push(&mut self, frame: &[i16]) -> Result<Option<f32>> {
        let Some(score) = self.model.push(frame)? else {
            return Ok(None);
        };

        if self.settling > 0 {
            self.settling -= 1;
            return Ok(None);
        }
        if score < self.threshold {
            return Ok(None);
        }

        // Heard it. Look away for a while so the same utterance is not
        // reported again as it drains out of the pipeline.
        self.model.reset();
        self.settling = self.settle_frames;
        Ok(Some(score))
    }

    /// Start again from nothing.
    pub fn reset(&mut self) {
        self.model.reset();
        self.settling = self.settle_frames;
    }
}

/// What the handle has asked the wake thread to do.
#[derive(Default)]
struct Control {
    threshold: Option<f32>,
    reset: bool,
}

/// Runs wake word detection away from the capture thread.
pub struct WakeThread {
    control: Arc<Mutex<Control>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl WakeThread {
    pub fn start(
        detector: Detector,
        ring: Arc<Ring<AudioChunk>>,
        dispatcher: Arc<Dispatcher>,
    ) -> Result<Self> {
        let control = Arc::new(Mutex::new(Control::default()));
        let stop = Arc::new(AtomicBool::new(false));

        let worker = {
            let control = Arc::clone(&control);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("edge-ear-wake".to_string())
                .spawn(move || run(detector, ring, control, stop, dispatcher))
                .map_err(|e| Error::Backend {
                    device: Device::Input,
                    reason: format!("wake thread would not start: {e}"),
                })?
        };

        Ok(Self {
            control,
            stop,
            worker: Some(worker),
        })
    }

    pub fn set_threshold(&self, value: f32) {
        self.lock().threshold = Some(value);
    }

    /// Forget what has been heard, so a fresh utterance is needed.
    pub fn reset(&self) {
        self.lock().reset = true;
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Control> {
        self.control.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for WakeThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(
    mut detector: Detector,
    ring: Arc<Ring<AudioChunk>>,
    control: Arc<Mutex<Control>>,
    stop: Arc<AtomicBool>,
    dispatcher: Arc<Dispatcher>,
) {
    while !stop.load(Ordering::Relaxed) {
        // Short waits, so being told to stop is noticed even when no
        // audio is arriving.
        let chunk = match ring.take(Some(Duration::from_millis(50))) {
            Ok(taken) => Some(taken.item),
            Err(Error::Timeout) => None,
            Err(_) => break,
        };

        {
            let mut c = control.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(threshold) = c.threshold.take() {
                detector.set_threshold(threshold);
            }
            if std::mem::take(&mut c.reset) {
                detector.reset();
            }
        }

        let Some(chunk) = chunk else { continue };
        let Some(samples) = chunk.samples.as_i16() else {
            continue;
        };

        match detector.push(samples) {
            Ok(Some(score)) => dispatcher.emit(Event::WakeDetected { score }),
            Ok(None) => {}
            Err(e) => dispatcher.emit(Event::DeviceError {
                device: Device::Input,
                message: e.to_string(),
            }),
        }
    }
}
