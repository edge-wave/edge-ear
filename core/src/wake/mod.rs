pub mod model;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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

    /// Feed one frame.
    ///
    /// The first value is the score, whenever the pipeline has heard
    /// enough to give one. The second says it counted as a detection.
    pub fn push(&mut self, frame: &[i16]) -> Result<(Option<f32>, bool)> {
        // Counted per frame of audio, not per score. Clearing the
        // pipeline stops it scoring at all until it refills, and a
        // count that only moved on scores would then wait for the
        // refill and only start afterwards, keeping the detector deaf
        // for about twice as long as intended.
        if self.settling > 0 {
            self.settling -= 1;
        }

        let Some(score) = self.model.push(frame)? else {
            return Ok((None, false));
        };
        if self.settling > 0 || score < self.threshold {
            return Ok((Some(score), false));
        }

        // Heard it. Clearing the pipeline drops the utterance that
        // caused this, so it cannot be heard a second time on its way
        // out. The count above covers the refill.
        self.model.reset();
        self.settling = self.settle_frames;
        Ok((Some(score), true))
    }

    /// Start again from nothing.
    pub fn reset(&mut self) {
        self.model.reset();
        self.settling = self.settle_frames;
    }

    /// Frames still to be ignored. Used by the test that checks the
    /// count moves while the pipeline is refilling.
    #[cfg(test)]
    pub fn settling(&self) -> u32 {
        self.settling
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wake::model::FRAME_SAMPLES;

    fn detector(settle: u32) -> Option<Detector> {
        let dir = std::path::PathBuf::from(std::env::var("EDGE_EAR_WAKE_DIR").ok()?);
        let mut model = WakeModel::new(
            &dir.join("melspectrogram.onnx"),
            &dir.join("embedding_model.onnx"),
        )
        .ok()?;
        model.load_word(&dir.join("hey_jarvis_v0.1.onnx")).ok()?;
        Some(Detector::new(model, 0.5, settle))
    }

    /// The count has to move while the pipeline is empty.
    ///
    /// Clearing the pipeline stops it scoring at all until it refills.
    /// A count that only moved on scores would wait for that refill and
    /// only begin afterwards, leaving the detector deaf for about twice
    /// as long as asked for.
    #[test]
    #[ignore]
    fn looking_away_is_counted_in_audio_not_in_scores() {
        let Some(mut detector) = detector(20) else {
            println!("set EDGE_EAR_WAKE_DIR to run this");
            return;
        };
        detector.reset();
        assert_eq!(detector.settling(), 20);

        let quiet = vec![0i16; FRAME_SAMPLES];

        // The pipeline gives nothing for the first several frames while
        // it refills. The count must move anyway.
        let (score, _) = detector.push(&quiet).unwrap();
        assert!(score.is_none(), "it should not be scoring yet");
        assert_eq!(detector.settling(), 19, "the count stalled on the refill");

        for _ in 0..19 {
            detector.push(&quiet).unwrap();
        }
        assert_eq!(detector.settling(), 0, "still looking away after 20 frames");
    }

    #[test]
    #[ignore]
    fn a_fresh_detector_is_not_looking_away() {
        let Some(detector) = detector(20) else { return };
        assert_eq!(detector.settling(), 0, "nothing has been heard yet");
    }
}

/// What the handle has asked the wake thread to do.
#[derive(Default)]
struct Control {
    threshold: Option<f32>,
    reset: bool,
}

/// Done the moment the wake word is heard, before the application is
/// told. This is where the alert plays and the recording opens, so a
/// slow application handler cannot delay either.
pub type OnWake = Box<dyn Fn() + Send>;

/// Stands for "nothing has been scored yet". Every real score is a
/// number between zero and one, so no score can collide with it.
const NO_SCORE: u32 = u32::MAX;

/// Runs wake word detection away from the capture thread.
pub struct WakeThread {
    control: Arc<Mutex<Control>>,
    stop: Arc<AtomicBool>,
    /// The most recent score, whether or not it counted as a detection.
    /// An application tuning how sure the detector must be needs to see
    /// the ones that fell short.
    last_score: Arc<AtomicU32>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl WakeThread {
    pub fn start(
        detector: Detector,
        ring: Arc<Ring<AudioChunk>>,
        dispatcher: Arc<Dispatcher>,
        on_wake: OnWake,
    ) -> Result<Self> {
        let control = Arc::new(Mutex::new(Control::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let last_score = Arc::new(AtomicU32::new(NO_SCORE));

        let worker = {
            let control = Arc::clone(&control);
            let stop = Arc::clone(&stop);
            let scores = Arc::clone(&last_score);
            thread::Builder::new()
                .name("edge-ear-wake".to_string())
                .spawn(move || run(detector, ring, control, stop, scores, dispatcher, on_wake))
                .map_err(|e| Error::Backend {
                    device: Device::Input,
                    reason: format!("wake thread would not start: {e}"),
                })?
        };

        Ok(Self {
            control,
            stop,
            last_score,
            worker: Mutex::new(Some(worker)),
        })
    }

    pub fn set_threshold(&self, value: f32) {
        self.lock().threshold = Some(value);
    }

    /// Forget what has been heard, so a fresh utterance is needed.
    pub fn reset(&self) {
        self.lock().reset = true;
    }

    /// The most recent score, or nothing until the pipeline has heard
    /// enough to give one.
    pub fn last_score(&self) -> Option<f32> {
        match self.last_score.load(Ordering::Relaxed) {
            NO_SCORE => None,
            bits => Some(f32::from_bits(bits)),
        }
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let worker = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(worker) = worker {
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
    scores: Arc<AtomicU32>,
    dispatcher: Arc<Dispatcher>,
    on_wake: OnWake,
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
            Ok((score, detected)) => {
                if let Some(score) = score {
                    scores.store(score.to_bits(), Ordering::Relaxed);
                }
                if detected {
                    // Act first, tell the application second. The alert
                    // and the recording must not wait on a handler.
                    on_wake();
                    dispatcher.emit(Event::WakeDetected {
                        score: score.unwrap_or(0.0),
                    });
                }
            }
            Err(e) => dispatcher.emit(Event::DeviceError {
                device: Device::Input,
                message: e.to_string(),
            }),
        }
    }
}
