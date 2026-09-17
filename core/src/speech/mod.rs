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
/// Collecting audio and counting silence are separate, because an alert
/// must not count as speech though its stretch may still be wanted.
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
    /// Audio gone by, kept so a recording can reach back into it.
    /// Trimmed to what the largest allowed pre-roll could ask for.
    history: Vec<i16>,
    history_limit: usize,
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
            history: Vec::new(),
            history_limit: 0,
        }
    }

    /// The most history any recording may reach back into.
    pub fn set_history_limit(&mut self, longest: Duration) {
        self.history_limit = self.samples_in(longest);
        let keep = self.history_limit;
        if self.history.len() > keep {
            let gone = self.history.len() - keep;
            self.history.drain(..gone);
        }
    }

    fn samples_in(&self, span: Duration) -> usize {
        let frames = span.as_secs_f64() * self.format.sample_rate as f64;
        frames as usize * self.format.channels.max(1) as usize
    }

    /// Begin a recording. `pre_roll` reaches back into audio gone by.
    /// `counting` is false while an alert plays: audio is kept, but
    /// silence waits for the sound to finish.
    pub fn open(&mut self, pre_roll: Duration, counting: bool) {
        // The model carries state between calls. Left alone, what it
        // heard during the last recording would colour this one.
        self.model.reset();
        let wanted = self.samples_in(pre_roll).min(self.history.len());
        self.audio = self.history[self.history.len() - wanted..].to_vec();
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
        // Kept whether or not a recording is open. The point of
        // history is having it before anyone asks.
        if self.history_limit > 0 {
            self.history.extend_from_slice(frame);
            if self.history.len() > self.history_limit {
                let gone = self.history.len() - self.history_limit;
                self.history.drain(..gone);
            }
        }
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
        d.open(Duration::ZERO, true);

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
        d.open(Duration::ZERO, true);

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
        d.open(Duration::ZERO, true);

        let done = run(&mut d, &limits(3_000, 320, 10_000));
        assert_eq!(done.reason, EndReason::MaxLength);
        assert!(done.duration >= Duration::from_millis(320));
    }

    #[test]
    fn saying_nothing_ends_on_the_no_speech_timeout() {
        let mut d = detector(ScriptedModel::silent());
        d.open(Duration::ZERO, true);

        let done = run(&mut d, &limits(3_000, 30_000, 320));
        assert_eq!(done.reason, EndReason::NoSpeech);
    }

    #[test]
    fn the_application_can_end_it_itself() {
        let mut d = detector(ScriptedModel::always_speaking());
        d.open(Duration::ZERO, true);
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
        d.open(Duration::ZERO, true);
        assert_eq!(d.model.resets, 1);

        let l = limits(320, 30_000, 10_000);
        let first = run(&mut d, &l);

        d.open(Duration::ZERO, true);
        assert_eq!(d.model.resets, 2, "the second recording reset it again");
        let second = run(&mut d, &l);

        // Same script, same answer. If state leaked, it would not be.
        assert_eq!(first.reason, second.reason);
        assert_eq!(first.audio.len(), second.audio.len());
    }

    #[test]
    fn audio_from_before_the_open_leads_the_recording() {
        let mut d = detector(ScriptedModel::speech_then_silence(2));
        d.set_history_limit(Duration::from_millis(100));

        // Heard before anyone asked for a recording.
        let earlier = [7i16; FRAME];
        d.push(&earlier, &limits(320, 30_000, 10_000)).unwrap();

        d.open(Duration::from_millis(FRAME as u64 / 16), true);
        let done = run(&mut d, &limits(320, 30_000, 10_000));
        assert_eq!(
            &done.audio[..FRAME],
            &earlier[..],
            "the earlier audio should lead"
        );
    }

    #[test]
    fn history_is_not_kept_when_no_recording_will_want_it() {
        let mut d = detector(ScriptedModel::always_speaking());
        d.push(&[7; FRAME], &limits(320, 30_000, 10_000)).unwrap();

        d.open(Duration::from_millis(500), true);
        let done = run(&mut d, &limits(320, 30_000, 10_000));
        assert!(
            !done.audio.starts_with(&[7; FRAME]),
            "kept history nobody had asked for"
        );
    }

    #[test]
    fn history_no_longer_than_asked_for_is_kept() {
        let mut d = detector(ScriptedModel::always_speaking());
        d.set_history_limit(Duration::from_millis(20));
        for _ in 0..10 {
            d.push(&[7; FRAME], &limits(320, 30_000, 10_000)).unwrap();
        }
        assert_eq!(d.history.len(), 320, "history outgrew what was allowed");
    }

    #[test]
    fn silence_is_not_counted_until_the_alert_sound_has_finished() {
        let mut d = detector(ScriptedModel::silent());
        // Opened while an alert is still playing.
        d.open(Duration::ZERO, false);

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
        d.open(Duration::ZERO, false);
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
        d.open(Duration::ZERO, false);
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

/// The handles the speech thread and its owner hold between them.
struct Shared {
    control: Arc<Mutex<Control>>,
    limits: Arc<Mutex<TunableConfig>>,
    stop: Arc<AtomicBool>,
    open: Arc<AtomicBool>,
}

/// What the handle has asked the speech thread to do.
#[derive(Default)]
struct Control {
    /// A recording should open, with this audio in front of it.
    open: Option<(Duration, bool)>,
    /// The open recording should end now.
    stop: bool,
    /// The alert sound has finished; start counting silence.
    start_counting: bool,
}

/// Runs speech detection away from the capture thread. Inference takes
/// as long as it takes, and doing it where audio is read would stall
/// every other consumer, so it happens here from its own queue.
pub struct SpeechThread {
    control: Arc<Mutex<Control>>,
    stop: Arc<AtomicBool>,
    /// True while a recording is collecting. Read by the handle, which
    /// refuses to change the rules a running recording is following.
    open: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl SpeechThread {
    pub fn start(
        ring: Arc<Ring<AudioChunk>>,
        format: AudioFormat,
        limits: Arc<Mutex<TunableConfig>>,
        dispatcher: Arc<Dispatcher>,
    ) -> Result<Self> {
        let model = SileroModel::bundled(format)?;
        let control = Arc::new(Mutex::new(Control::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let open = Arc::new(AtomicBool::new(false));

        let worker = {
            let shared = Shared {
                control: Arc::clone(&control),
                limits,
                stop: Arc::clone(&stop),
                open: Arc::clone(&open),
            };
            thread::Builder::new()
                .name("edge-ear-speech".to_string())
                .spawn(move || run(model, format, ring, shared, dispatcher))
                .map_err(|e| crate::error::Error::Backend {
                    device: crate::config::Device::Input,
                    reason: format!("speech thread would not start: {e}"),
                })?
        };

        Ok(Self {
            control,
            stop,
            open,
            worker: Mutex::new(Some(worker)),
        })
    }

    pub fn open_recording(&self, pre_roll: Duration, counting: bool) {
        self.lock().open = Some((pre_roll, counting));
    }

    pub fn stop_recording(&self) {
        self.lock().stop = true;
    }

    /// True while a recording is collecting audio.
    pub fn is_recording(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    /// Told when an alert sound has ended, so the tail of the alert is
    /// never counted as speech.
    pub fn start_counting(&self) {
        self.lock().start_counting = true;
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let worker = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(worker) = worker {
            crate::join_worker(worker, "speech");
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
    shared: Shared,
    dispatcher: Arc<Dispatcher>,
) {
    let Shared {
        control,
        limits,
        stop,
        open,
    } = shared;
    let mut detector = Detector::new(model, format);
    // An inference failure is logged when it starts, not on every frame.
    let mut failing = false;

    while !stop.load(Ordering::Relaxed) {
        // Short waits, so being told to stop is noticed promptly even
        // when no audio is arriving.
        let chunk = match ring.take(Some(Duration::from_millis(50))) {
            Ok(taken) => Some(taken.item),
            Err(crate::error::Error::Timeout) => None,
            Err(_) => break,
        };

        let (opening, should_stop, counting) = {
            let mut c = control.lock().unwrap_or_else(|e| e.into_inner());
            (
                c.open.take(),
                std::mem::take(&mut c.stop),
                std::mem::take(&mut c.start_counting),
            )
        };
        // Read fresh every pass, so a setting changed while running is
        // followed by whichever path opened the recording.
        let limits = limits.lock().unwrap_or_else(|e| e.into_inner()).clone();

        detector.set_history_limit(limits.pre_roll);
        if let Some((pre_roll, counting_now)) = opening {
            log::debug!("recording opened: pre-roll {pre_roll:?}, counting silence {counting_now}");
            detector.open(pre_roll, counting_now);
            open.store(true, Ordering::Relaxed);
        }
        if counting {
            log::debug!("the alert ended, so silence now counts");
            detector.start_counting();
        }
        if should_stop && let Some(done) = detector.stop() {
            open.store(false, Ordering::Relaxed);
            emit(&dispatcher, done);
            continue;
        }

        let Some(chunk) = chunk else { continue };
        let Some(samples) = chunk.samples.as_i16() else {
            continue;
        };
        let result = detector.push(samples, &limits);
        if failing && result.is_ok() {
            log::info!("speech detection works again");
            failing = false;
        }
        match result {
            Ok(Some(done)) => {
                open.store(false, Ordering::Relaxed);
                emit(&dispatcher, done);
            }
            Ok(None) => {}
            Err(e) => {
                if !failing {
                    log::error!("speech detection failed: {e}");
                    failing = true;
                }
                dispatcher.emit(Event::DeviceError {
                    device: crate::config::Device::Input,
                    message: e.to_string(),
                });
            }
        }
    }
    open.store(false, Ordering::Relaxed);
}

fn emit(dispatcher: &Dispatcher, done: Recording) {
    log::info!(
        "recording ended: {}, {:?} of audio",
        done.reason,
        done.duration
    );
    dispatcher.emit(Event::SpeechEnded {
        audio: done.audio,
        sample_rate: done.sample_rate,
        reason: done.reason,
        duration: done.duration,
    });
}
