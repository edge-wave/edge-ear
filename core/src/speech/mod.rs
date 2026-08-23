pub mod model;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::capture::AudioChunk;
use crate::capture::ring::Ring;
use crate::config::{AudioFormat, TunableConfig};
use crate::error::Result;
use crate::events::dispatch::Dispatcher;
use crate::events::{EndReason, Event};
use crate::speech::model::{SileroModel, SpeechModel};

/// Audio collected for one recording, and why it ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Recording {
    pub audio: Vec<i16>,
    pub reason: EndReason,
    pub duration: Duration,
    pub sample_rate: u32,
}

/// Tracks one recording from the moment it opens to the moment it ends.
///
/// Collecting audio and counting silence are deliberately separate. An
/// alert sound played on wake must not be counted as speech, but the
/// audio from that stretch may still be wanted, so one can start before
/// the other.
pub struct Detector<M: SpeechModel> {
    model: M,
    format: AudioFormat,
    audio: Vec<i16>,
    /// False until the alert sound, if any, has finished. Silence is not
    /// counted before then.
    counting: bool,
    speech_seen: bool,
    silence_run: Duration,
    elapsed: Duration,
    open: bool,
}

impl<M: SpeechModel> Detector<M> {
    pub fn new(model: M, format: AudioFormat) -> Self {
        Self {
            model,
            format,
            audio: Vec::new(),
            counting: true,
            speech_seen: false,
            silence_run: Duration::ZERO,
            elapsed: Duration::ZERO,
            open: false,
        }
    }

    /// Begin a recording.
    ///
    /// `pre_roll` is audio from before this moment, already collected
    /// elsewhere. `counting` is false when an alert sound is still
    /// playing: audio is kept, but silence is not counted until the
    /// sound has finished.
    pub fn open(&mut self, pre_roll: Vec<i16>, counting: bool) {
        // The model carries state between calls. Left alone, what it
        // heard during the last recording would colour this one.
        self.model.reset();
        self.audio = pre_roll;
        self.counting = counting;
        self.speech_seen = false;
        self.silence_run = Duration::ZERO;
        self.elapsed = Duration::ZERO;
        self.open = true;
    }

    /// The alert sound has finished; start counting silence.
    pub fn start_counting(&mut self) {
        self.counting = true;
    }

    #[cfg(test)]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Feed one frame. Returns the finished recording when this frame
    /// ended it.
    pub fn push(&mut self, frame: &[i16], limits: &TunableConfig) -> Result<Option<Recording>> {
        if !self.open {
            return Ok(None);
        }

        self.audio.extend_from_slice(frame);
        let frame_time = self.frame_duration(frame.len());
        self.elapsed += frame_time;

        if self.counting {
            let probability = self.model.probability(frame)?;
            if probability >= limits.speech_threshold {
                self.speech_seen = true;
                self.silence_run = Duration::ZERO;
            } else {
                self.silence_run += frame_time;
            }
        }

        Ok(self.verdict(limits))
    }

    /// End the recording because the application said so.
    pub fn stop(&mut self) -> Option<Recording> {
        if !self.open {
            return None;
        }
        Some(self.finish(EndReason::Stopped))
    }

    fn verdict(&mut self, limits: &TunableConfig) -> Option<Recording> {
        // The length cap comes first: it holds whatever else is true, and
        // it is what keeps memory from growing while someone talks on.
        if self.elapsed >= limits.max_recording {
            return Some(self.finish(EndReason::MaxLength));
        }
        if !self.counting {
            return None;
        }
        if self.speech_seen {
            if self.silence_run >= limits.silence_duration {
                return Some(self.finish(EndReason::Silence));
            }
        } else if self.elapsed >= limits.no_speech_timeout {
            return Some(self.finish(EndReason::NoSpeech));
        }
        None
    }

    fn finish(&mut self, reason: EndReason) -> Recording {
        self.open = false;
        Recording {
            audio: std::mem::take(&mut self.audio),
            reason,
            duration: self.elapsed,
            sample_rate: self.format.sample_rate,
        }
    }

    fn frame_duration(&self, samples: usize) -> Duration {
        let frames = samples / self.format.channels.max(1) as usize;
        Duration::from_secs_f64(frames as f64 / self.format.sample_rate as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech::model::ScriptedModel;

    /// 512 samples is one frame at 16 kHz: 32 ms.
    const FRAME: usize = 512;

    fn limits(silence_ms: u64, max_ms: u64, no_speech_ms: u64) -> TunableConfig {
        TunableConfig {
            silence_duration: Duration::from_millis(silence_ms),
            max_recording: Duration::from_millis(max_ms),
            no_speech_timeout: Duration::from_millis(no_speech_ms),
            ..Default::default()
        }
    }

    fn detector(model: ScriptedModel) -> Detector<ScriptedModel> {
        Detector::new(model, AudioFormat::mono_16k())
    }

    /// Feed frames until the recording ends, or give up.
    fn run(d: &mut Detector<ScriptedModel>, limits: &TunableConfig) -> Recording {
        for _ in 0..2000 {
            if let Some(done) = d.push(&[100; FRAME], limits).unwrap() {
                return done;
            }
        }
        panic!("the recording never ended");
    }

    #[test]
    fn speech_then_quiet_ends_on_silence() {
        // Ten frames of speech, then quiet.
        let mut d = detector(ScriptedModel::speech_then_silence(10));
        d.open(Vec::new(), true);

        let done = run(&mut d, &limits(320, 30_000, 10_000));
        assert_eq!(done.reason, EndReason::Silence);
        assert!(!d.is_open(), "the recording closed");
        // Ten frames of speech plus the ten of silence it waited through.
        assert!(done.audio.len() >= 20 * FRAME, "got {}", done.audio.len());
    }

    #[test]
    fn a_short_pause_does_not_end_it_and_stays_in_the_audio() {
        // Speech, a two frame gap, speech again, then quiet.
        let mut script = vec![0.9, 0.9, 0.0, 0.0, 0.9, 0.9];
        script.push(0.0);
        let mut d = detector(ScriptedModel::new(script));
        d.open(Vec::new(), true);

        // The gap is shorter than the silence the detector waits for.
        let done = run(&mut d, &limits(320, 30_000, 10_000));
        assert_eq!(done.reason, EndReason::Silence);
        // Everything before the end is kept, gap included.
        assert!(
            done.audio.len() > 6 * FRAME,
            "the pause was dropped: {}",
            done.audio.len()
        );
    }

    #[test]
    fn talking_past_the_cap_ends_on_length() {
        let mut d = detector(ScriptedModel::always_speaking());
        d.open(Vec::new(), true);

        let done = run(&mut d, &limits(3_000, 320, 10_000));
        assert_eq!(done.reason, EndReason::MaxLength);
        assert!(done.duration >= Duration::from_millis(320));
    }

    #[test]
    fn saying_nothing_ends_on_the_no_speech_timeout() {
        let mut d = detector(ScriptedModel::silent());
        d.open(Vec::new(), true);

        let done = run(&mut d, &limits(3_000, 30_000, 320));
        assert_eq!(done.reason, EndReason::NoSpeech);
    }

    #[test]
    fn the_application_can_end_it_itself() {
        let mut d = detector(ScriptedModel::always_speaking());
        d.open(Vec::new(), true);
        d.push(&[100; FRAME], &limits(3_000, 30_000, 10_000))
            .unwrap();

        let done = d.stop().expect("a recording was open");
        assert_eq!(done.reason, EndReason::Stopped);
        assert!(!d.is_open());
        assert!(d.stop().is_none(), "stopping twice gives nothing");
    }

    #[test]
    fn opening_a_recording_makes_the_model_forget() {
        let mut d = detector(ScriptedModel::speech_then_silence(4));
        d.open(Vec::new(), true);
        assert_eq!(d.model.resets, 1);

        let l = limits(320, 30_000, 10_000);
        let first = run(&mut d, &l);

        d.open(Vec::new(), true);
        assert_eq!(d.model.resets, 2, "the second recording reset it again");
        let second = run(&mut d, &l);

        // Same script, same answer. If state leaked, it would not be.
        assert_eq!(first.reason, second.reason);
        assert_eq!(first.audio.len(), second.audio.len());
    }

    #[test]
    fn pre_roll_audio_is_kept_at_the_front() {
        let mut d = detector(ScriptedModel::speech_then_silence(2));
        let earlier = vec![7i16; 1000];
        d.open(earlier.clone(), true);

        let done = run(&mut d, &limits(320, 30_000, 10_000));
        assert_eq!(
            &done.audio[..1000],
            &earlier[..],
            "the earlier audio should lead"
        );
    }

    #[test]
    fn silence_is_not_counted_until_the_alert_sound_has_finished() {
        let mut d = detector(ScriptedModel::silent());
        // Opened while an alert is still playing.
        d.open(Vec::new(), false);

        let l = limits(320, 30_000, 320);
        // Far past every limit, but nothing is being counted yet.
        for _ in 0..50 {
            assert!(
                d.push(&[100; FRAME], &l).unwrap().is_none(),
                "nothing should end while the alert is still playing"
            );
        }
        assert!(d.is_open());

        // The alert finishes. Now the quiet counts.
        d.start_counting();
        let done = run(&mut d, &l);
        assert_eq!(done.reason, EndReason::NoSpeech);
    }

    #[test]
    fn audio_is_still_collected_while_the_alert_plays() {
        let mut d = detector(ScriptedModel::silent());
        d.open(Vec::new(), false);
        for _ in 0..5 {
            d.push(&[100; FRAME], &limits(320, 30_000, 10_000)).unwrap();
        }
        d.start_counting();
        let done = run(&mut d, &limits(320, 30_000, 320));
        assert!(
            done.audio.len() >= 5 * FRAME,
            "audio from the alert stretch was dropped"
        );
    }

    #[test]
    fn the_length_cap_holds_even_while_the_alert_plays() {
        let mut d = detector(ScriptedModel::always_speaking());
        d.open(Vec::new(), false);
        // Nothing is being counted, but memory must still not grow
        // without limit.
        let done = run(&mut d, &limits(3_000, 320, 10_000));
        assert_eq!(done.reason, EndReason::MaxLength);
    }

    #[test]
    fn a_closed_detector_ignores_frames() {
        let mut d = detector(ScriptedModel::always_speaking());
        assert!(
            d.push(&[100; FRAME], &limits(320, 30_000, 10_000))
                .unwrap()
                .is_none()
        );
        assert!(!d.is_open());
    }
}

/// What the handle has asked the speech thread to do.
#[derive(Default)]
struct Control {
    /// A recording should open, with this audio in front of it.
    open: Option<(Vec<i16>, bool)>,
    /// The open recording should end now.
    stop: bool,
    /// The alert sound has finished; start counting silence.
    start_counting: bool,
    limits: TunableConfig,
}

/// Runs speech detection away from the capture thread.
///
/// Detection is inference, and inference takes as long as it takes.
/// Doing it where audio is read would stall every other consumer, so it
/// happens here instead, reading from its own queue.
pub struct SpeechThread {
    control: Arc<Mutex<Control>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl SpeechThread {
    pub fn start(
        ring: Arc<Ring<AudioChunk>>,
        format: AudioFormat,
        limits: TunableConfig,
        dispatcher: Arc<Dispatcher>,
    ) -> Result<Self> {
        let model = SileroModel::bundled(format)?;
        let control = Arc::new(Mutex::new(Control {
            limits,
            ..Default::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));

        let worker = {
            let control = Arc::clone(&control);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("edge-ear-speech".to_string())
                .spawn(move || run(model, format, ring, control, stop, dispatcher))
                .map_err(|e| crate::error::Error::Backend {
                    device: crate::config::Device::Input,
                    reason: format!("speech thread would not start: {e}"),
                })?
        };

        Ok(Self {
            control,
            stop,
            worker: Some(worker),
        })
    }

    pub fn open_recording(&self, pre_roll: Vec<i16>, counting: bool) {
        self.lock().open = Some((pre_roll, counting));
    }

    pub fn stop_recording(&self) {
        self.lock().stop = true;
    }

    /// Told when an alert sound has finished, so the tail of the alert
    /// is never counted as speech. Nothing plays an alert on wake yet;
    /// the work that joins wake, playback, and recording will call this.
    #[allow(dead_code, reason = "the alert gate is wired up with wake")]
    pub fn start_counting(&self) {
        self.lock().start_counting = true;
    }

    pub fn set_limits(&self, limits: TunableConfig) {
        self.lock().limits = limits;
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

impl Drop for SpeechThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(
    model: SileroModel,
    format: AudioFormat,
    ring: Arc<Ring<AudioChunk>>,
    control: Arc<Mutex<Control>>,
    stop: Arc<AtomicBool>,
    dispatcher: Arc<Dispatcher>,
) {
    let mut detector = Detector::new(model, format);

    while !stop.load(Ordering::Relaxed) {
        // Short waits, so being told to stop is noticed promptly even
        // when no audio is arriving.
        let chunk = match ring.take(Some(Duration::from_millis(50))) {
            Ok(taken) => Some(taken.item),
            Err(crate::error::Error::Timeout) => None,
            Err(_) => break,
        };

        let (open, should_stop, counting, limits) = {
            let mut c = control.lock().unwrap_or_else(|e| e.into_inner());
            (
                c.open.take(),
                std::mem::take(&mut c.stop),
                std::mem::take(&mut c.start_counting),
                c.limits.clone(),
            )
        };

        if let Some((pre_roll, counting_now)) = open {
            detector.open(pre_roll, counting_now);
        }
        if counting {
            detector.start_counting();
        }
        if should_stop && let Some(done) = detector.stop() {
            emit(&dispatcher, done);
            continue;
        }

        let Some(chunk) = chunk else { continue };
        let Some(samples) = chunk.samples.as_i16() else {
            continue;
        };
        match detector.push(samples, &limits) {
            Ok(Some(done)) => emit(&dispatcher, done),
            Ok(None) => {}
            Err(e) => dispatcher.emit(Event::DeviceError {
                device: crate::config::Device::Input,
                message: e.to_string(),
            }),
        }
    }
}

fn emit(dispatcher: &Dispatcher, done: Recording) {
    dispatcher.emit(Event::SpeechEnded {
        audio: done.audio,
        sample_rate: done.sample_rate,
        reason: done.reason,
        duration: done.duration,
    });
}
