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
use crate::wake::model::WakeSource;

/// Decides when a score counts as hearing the wake word.
///
/// After one detection it ignores what follows for a while. Clearing
/// the score is not enough on its own: the audio that caused the
/// detection is still inside the pipeline and would cause another one
/// immediately.
pub struct Detector<M: WakeSource> {
    model: M,
    threshold: f32,
    settle_frames: u32,
    settling: u32,
}

impl<M: WakeSource> Detector<M> {
    pub fn new(model: M, threshold: f32, settle_frames: u32) -> Self {
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
        let looking_away = self.settling > 0;
        if looking_away {
            self.settling -= 1;
        }

        let Some(score) = self.model.push(frame)? else {
            return Ok((None, false));
        };
        if looking_away || score < self.threshold {
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
    pub fn start<M: WakeSource + 'static>(
        detector: Detector<M>,
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

fn run<M: WakeSource>(
    mut detector: Detector<M>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{AudioChunk, Samples};
    use crate::config::AudioFormat;
    use crate::events::dispatch::DEFAULT_QUEUE_CAPACITY;
    use crate::wake::model::ScriptedWake;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    const FRAME: usize = 1280;

    fn detector(script: ScriptedWake, threshold: f32, settle: u32) -> Detector<ScriptedWake> {
        Detector::new(script, threshold, settle)
    }

    fn push(d: &mut Detector<ScriptedWake>) -> (Option<f32>, bool) {
        d.push(&[100; FRAME]).unwrap()
    }

    fn chunk() -> AudioChunk {
        AudioChunk::new(
            Samples::I16(vec![100; FRAME]),
            AudioFormat::mono_16k(),
            Instant::now(),
        )
    }

    fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn a_score_over_the_threshold_counts() {
        let mut d = detector(ScriptedWake::heard_after(2, 0.9), 0.5, 20);
        assert_eq!(push(&mut d), (Some(0.05), false));
        assert_eq!(push(&mut d), (Some(0.05), false));
        assert_eq!(push(&mut d), (Some(0.9), true));
    }

    #[test]
    fn a_score_just_under_the_threshold_does_not() {
        let mut d = detector(ScriptedWake::heard_after(1, 0.49), 0.5, 20);
        push(&mut d);
        assert_eq!(push(&mut d), (Some(0.49), false));
    }

    #[test]
    fn a_score_exactly_at_the_threshold_counts() {
        let mut d = detector(ScriptedWake::heard_after(1, 0.5), 0.5, 20);
        push(&mut d);
        assert_eq!(push(&mut d), (Some(0.5), true));
    }

    #[test]
    fn scores_that_fall_short_are_still_reported() {
        // Choosing how sure the detector must be is guesswork without
        // seeing the ones that did not make it.
        let mut d = detector(ScriptedWake::heard_after(3, 0.9), 0.5, 20);
        for _ in 0..3 {
            assert_eq!(push(&mut d), (Some(0.05), false));
        }
    }

    /// The count has to move while the pipeline is empty.
    ///
    /// Hearing the word clears the pipeline, and a cleared pipeline
    /// scores nothing until it refills. A count that only moved on
    /// scores would wait out that refill and only then begin, leaving
    /// the detector deaf for about twice the spell it was given.
    #[test]
    fn looking_away_is_counted_in_audio_not_in_scores() {
        let mut d = detector(ScriptedWake::silent_then(10, 0.9), 0.5, 4);

        // Nothing scored yet, so nothing counts.
        for _ in 0..10 {
            assert_eq!(push(&mut d), (None, false));
        }
        assert_eq!(d.settling(), 0);

        let (_, counted) = push(&mut d);
        assert!(counted, "the loud frame should have counted");
        assert_eq!(d.settling(), 4);

        // Cleared, so it says nothing for ten frames. The count must
        // run through them rather than waiting for them to end.
        for expected in [3, 2, 1, 0] {
            assert_eq!(push(&mut d), (None, false));
            assert_eq!(d.settling(), expected, "the count stalled on the refill");
        }
    }

    #[test]
    fn the_same_words_are_not_heard_twice_on_the_way_out() {
        // Loud every frame, as one long utterance draining out would be.
        let mut d = detector(ScriptedWake::new(vec![Some(0.9)]), 0.5, 5);
        assert!(push(&mut d).1, "the first one counts");
        for _ in 0..5 {
            assert!(!push(&mut d).1, "it was heard again while looking away");
        }
        assert!(push(&mut d).1, "a fresh one counts once the spell is over");
    }

    #[test]
    fn hearing_it_clears_the_pipeline() {
        let mut d = detector(ScriptedWake::heard_after(1, 0.9), 0.5, 2);
        push(&mut d);
        push(&mut d);
        assert_eq!(d.model.resets, 1, "the pipeline was not cleared");
    }

    #[test]
    fn a_fresh_detector_is_not_looking_away() {
        let d = detector(ScriptedWake::new(vec![Some(0.1)]), 0.5, 20);
        assert_eq!(d.settling(), 0);
    }

    #[test]
    fn asking_it_to_start_again_makes_it_look_away() {
        let mut d = detector(ScriptedWake::new(vec![Some(0.9)]), 0.5, 3);
        d.reset();
        assert_eq!(d.settling(), 3);
        assert_eq!(d.model.resets, 1);
        assert!(!push(&mut d).1, "loud, but it is looking away");
    }

    #[test]
    fn a_threshold_changed_later_is_the_one_used() {
        let mut d = detector(ScriptedWake::new(vec![Some(0.6)]), 0.9, 20);
        assert!(!push(&mut d).1, "0.6 is under 0.9");
        d.set_threshold(0.5);
        assert!(push(&mut d).1, "0.6 is over 0.5");
    }

    /// What happens the moment it counts must happen before the
    /// application is told, so a slow handler cannot delay the alert or
    /// the recording.
    #[test]
    fn acting_on_it_comes_before_telling_anyone() {
        let ring = Arc::new(Ring::new(64));
        let dispatcher = Arc::new(Dispatcher::new(DEFAULT_QUEUE_CAPACITY));
        let acted = Arc::new(AtomicBool::new(false));
        let told_too_early = Arc::new(AtomicBool::new(false));

        let flag = Arc::clone(&acted);
        let bad = Arc::clone(&told_too_early);
        dispatcher.set_handler(Box::new(move |event| {
            if matches!(event, Event::WakeDetected { .. }) && !flag.load(Ordering::SeqCst) {
                bad.store(true, Ordering::SeqCst);
            }
        }));

        let on_wake = {
            let flag = Arc::clone(&acted);
            Box::new(move || flag.store(true, Ordering::SeqCst)) as OnWake
        };
        let thread = WakeThread::start(
            detector(ScriptedWake::heard_after(1, 0.9), 0.5, 20),
            Arc::clone(&ring),
            Arc::clone(&dispatcher),
            on_wake,
        )
        .unwrap();

        for _ in 0..4 {
            ring.push(chunk());
        }
        assert!(
            wait_until(|| acted.load(Ordering::SeqCst)),
            "nothing was done on hearing it"
        );
        assert!(
            !told_too_early.load(Ordering::SeqCst),
            "the application was told before the alert and recording were seen to"
        );
        thread.shutdown();
    }

    #[test]
    fn the_most_recent_score_is_there_to_read() {
        let ring = Arc::new(Ring::new(64));
        let dispatcher = Arc::new(Dispatcher::new(DEFAULT_QUEUE_CAPACITY));
        let thread = WakeThread::start(
            detector(ScriptedWake::new(vec![Some(0.31)]), 0.9, 20),
            Arc::clone(&ring),
            dispatcher,
            Box::new(|| {}),
        )
        .unwrap();

        assert_eq!(thread.last_score(), None, "nothing scored yet");
        ring.push(chunk());

        assert!(wait_until(|| thread.last_score().is_some()));
        // Under the threshold, so it never counted, but it is readable.
        assert_eq!(thread.last_score(), Some(0.31));
        thread.shutdown();
    }
}
