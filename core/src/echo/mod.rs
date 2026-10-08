//! Taking what the speaker played back out of the microphone. Output callbacks copy what they
//! hand the device into a reference, and the capture thread feeds both sides to a canceller.

#[cfg(feature = "webrtc-aec")]
pub mod webrtc;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::capture::Samples;
use crate::capture::convert::Converter;
use crate::capture::ring::Ring;
use crate::config::{AudioFormat, SampleType};
use crate::error::Result;

/// Removes from the microphone what it heard of the speaker. Both sides arrive as mono f32, 10 ms a
/// frame, each speaker frame before its echo; finding the exact delay is the canceller's job.
pub trait EchoCanceller: Send {
    /// The rate both sides arrive at.
    fn sample_rate(&self) -> u32;

    /// One frame of what the speaker played, silence included.
    fn render(&mut self, frame: &[f32]);

    /// One frame of the microphone, cleaned in place.
    fn capture(&mut self, frame: &mut [f32]);

    /// Forget the echo path learned so far. Called each time capture starts.
    fn reset(&mut self) {}
}

/// Which canceller edge-ear runs itself, on its own playback only. A system canceller, such as
/// PipeWire's echo-cancel source, is chosen as a device instead, with this left `Off`.
pub enum BuiltinCanceller {
    Off,
    /// WebRTC's AEC3, which needs the `webrtc-aec` feature.
    Webrtc,
    /// The application's own, kept across starts and reset at each one.
    Custom(Box<dyn EchoCanceller>),
}

/// Which built-in canceller is set, without the canceller itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancellerKind {
    Off,
    Webrtc,
    Custom,
}

/// How long before it is heard a speaker block is handed over. It covers a delay reported a
/// little long and what resampling holds back, and stays far inside what a canceller searches.
const RENDER_LEAD: Duration = Duration::from_millis(50);

/// Blocks of speaker audio kept for the capture thread, a few seconds at any callback size.
const REFERENCE_BLOCKS: usize = 1024;

/// One block the speaker took, as it took it.
struct Played {
    samples: Samples,
    format: AudioFormat,
    heard_at: Instant,
}

/// What the speaker played, kept for the echo canceller. Output streams fill it from their callbacks.
pub struct EchoReference {
    wanted: AtomicBool,
    played: Ring<Played>,
}

impl EchoReference {
    pub(crate) fn new() -> Self {
        Self {
            wanted: AtomicBool::new(false),
            played: Ring::new(REFERENCE_BLOCKS),
        }
    }

    /// Whether a canceller is running. A callback checks this before copying anything.
    pub fn is_wanted(&self) -> bool {
        self.wanted.load(Ordering::Relaxed)
    }

    /// Everything a device took in one go, silence included, the first sample heard at `heard_at`.
    pub fn push(&self, samples: Samples, format: AudioFormat, heard_at: Instant) {
        if self.is_wanted() && !samples.is_empty() {
            self.played.push(Played {
                samples,
                format,
                heard_at,
            });
        }
    }

    fn set_wanted(&self, on: bool) {
        self.wanted.store(on, Ordering::Relaxed);
        self.played.edit(VecDeque::clear);
    }
}

/// The step between the microphone and the consumers while echo is being cancelled.
pub(crate) struct EchoStage {
    canceller: Arc<Mutex<Box<dyn EchoCanceller>>>,
    reference: Arc<EchoReference>,
    format: AudioFormat,
    frame: usize,
    mic: Converter,
    mic_held: Vec<f32>,
    speaker: Option<(AudioFormat, Converter)>,
    speaker_held: Vec<f32>,
    /// Speaker blocks taken from the reference but not yet due.
    pending: VecDeque<Played>,
}

impl EchoStage {
    pub(crate) fn new(
        canceller: Arc<Mutex<Box<dyn EchoCanceller>>>,
        reference: Arc<EchoReference>,
        device: AudioFormat,
    ) -> Result<Self> {
        let rate = {
            let mut canceller = canceller.lock().unwrap_or_else(|e| e.into_inner());
            canceller.reset();
            canceller.sample_rate()
        };
        let format = AudioFormat::new(rate, 1, SampleType::F32);
        let mic = Converter::new(device, format)?;
        reference.set_wanted(true);
        Ok(Self {
            canceller,
            reference,
            format,
            frame: (rate as usize / 100).max(1),
            mic,
            mic_held: Vec::new(),
            speaker: None,
            speaker_held: Vec::new(),
            pending: VecDeque::new(),
        })
    }

    /// What [`Self::process`] hands back: one channel of f32 at the canceller's rate.
    pub(crate) fn format(&self) -> AudioFormat {
        self.format
    }

    /// Clean one microphone block, after handing over the speaker audio heard by now.
    pub(crate) fn process(&mut self, block: &Samples) -> Result<Samples> {
        let due = Instant::now() + RENDER_LEAD;
        let pending = &mut self.pending;
        self.reference
            .played
            .edit(|played| pending.extend(played.drain(..)));

        while self.pending.front().is_some_and(|p| p.heard_at <= due) {
            let played = self.pending.pop_front().expect("checked above");
            let samples = self
                .speaker_converter(played.format)?
                .convert(&played.samples)?;
            self.speaker_held
                .extend_from_slice(samples.as_f32().unwrap_or_default());
        }
        let samples = self.mic.convert(block)?;
        self.mic_held
            .extend_from_slice(samples.as_f32().unwrap_or_default());

        let mut canceller = self.canceller.lock().unwrap_or_else(|e| e.into_inner());
        let used = self.speaker_held.len() / self.frame * self.frame;
        for frame in self
            .speaker_held
            .drain(..used)
            .as_slice()
            .chunks_exact(self.frame)
        {
            canceller.render(frame);
        }
        let used = self.mic_held.len() / self.frame * self.frame;
        let mut cleaned: Vec<f32> = self.mic_held.drain(..used).collect();
        for frame in cleaned.chunks_exact_mut(self.frame) {
            canceller.capture(frame);
        }
        Ok(Samples::F32(cleaned))
    }

    /// One converter per speaker format, made again if the speaker reopens differently.
    fn speaker_converter(&mut self, from: AudioFormat) -> Result<&mut Converter> {
        if self
            .speaker
            .as_ref()
            .is_none_or(|(format, _)| *format != from)
        {
            self.speaker = Some((from, Converter::new(from, self.format)?));
            self.speaker_held.clear();
        }
        Ok(&mut self.speaker.as_mut().expect("set above").1)
    }
}

impl Drop for EchoStage {
    fn drop(&mut self) {
        self.reference.set_wanted(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes down what it is handed, and zeroes the microphone so the cleaning shows.
    #[derive(Default)]
    struct Log {
        rendered: Vec<Vec<f32>>,
        captured: Vec<Vec<f32>>,
        order: Vec<char>,
        resets: usize,
    }

    struct Recorder(Arc<Mutex<Log>>);

    impl EchoCanceller for Recorder {
        fn sample_rate(&self) -> u32 {
            16_000
        }
        fn render(&mut self, frame: &[f32]) {
            let mut log = self.0.lock().unwrap();
            log.rendered.push(frame.to_vec());
            log.order.push('r');
        }
        fn capture(&mut self, frame: &mut [f32]) {
            let mut log = self.0.lock().unwrap();
            log.captured.push(frame.to_vec());
            log.order.push('c');
            frame.fill(0.0);
        }
        fn reset(&mut self) {
            self.0.lock().unwrap().resets += 1;
        }
    }

    fn stage(device: AudioFormat) -> (EchoStage, Arc<EchoReference>, Arc<Mutex<Log>>) {
        let log = Arc::new(Mutex::new(Log::default()));
        let canceller: Box<dyn EchoCanceller> = Box::new(Recorder(Arc::clone(&log)));
        let reference = Arc::new(EchoReference::new());
        let stage = EchoStage::new(
            Arc::new(Mutex::new(canceller)),
            Arc::clone(&reference),
            device,
        )
        .unwrap();
        (stage, reference, log)
    }

    #[test]
    fn the_microphone_comes_out_cleaned_in_whole_frames() {
        let (mut stage, _, log) = stage(AudioFormat::mono_16k());
        assert_eq!(log.lock().unwrap().resets, 1);

        let out = stage.process(&Samples::I16(vec![16_384; 250])).unwrap();
        assert_eq!(out, Samples::F32(vec![0.0; 160]));
        let out = stage.process(&Samples::I16(vec![16_384; 70])).unwrap();
        assert_eq!(out, Samples::F32(vec![0.0; 160]));
        assert_eq!(log.lock().unwrap().captured, vec![vec![0.5; 160]; 2]);
    }

    #[test]
    fn speaker_audio_heard_by_now_goes_first() {
        let (mut stage, reference, log) = stage(AudioFormat::mono_16k());
        let now = Instant::now();
        reference.push(Samples::I16(vec![8_192; 320]), AudioFormat::mono_16k(), now);
        stage.process(&Samples::I16(vec![0; 160])).unwrap();

        let log = log.lock().unwrap();
        assert_eq!(log.order, vec!['r', 'r', 'c']);
        assert_eq!(log.rendered, vec![vec![0.25; 160]; 2]);
    }

    #[test]
    fn speaker_audio_not_yet_heard_waits_for_its_time() {
        let (mut stage, reference, log) = stage(AudioFormat::mono_16k());
        let later = Instant::now() + Duration::from_secs(5);
        reference.push(Samples::I16(vec![1; 160]), AudioFormat::mono_16k(), later);
        stage.process(&Samples::I16(vec![0; 160])).unwrap();

        assert_eq!(log.lock().unwrap().order, vec!['c']);
        assert_eq!(stage.pending.len(), 1);
    }

    #[test]
    fn a_stereo_speaker_is_brought_to_the_canceller_format() {
        let (mut stage, reference, log) = stage(AudioFormat::mono_16k());
        let stereo = AudioFormat::new(16_000, 2, SampleType::F32);
        let frames: Vec<f32> = [0.5, 0.25].repeat(160);
        reference.push(Samples::F32(frames), stereo, Instant::now());
        stage.process(&Samples::I16(vec![0; 160])).unwrap();

        assert_eq!(log.lock().unwrap().rendered, vec![vec![0.375; 160]]);
    }

    #[test]
    fn nothing_is_kept_while_no_canceller_runs() {
        let (stage, reference, _) = stage(AudioFormat::mono_16k());
        drop(stage);
        assert!(!reference.is_wanted());
        reference.push(
            Samples::I16(vec![1; 160]),
            AudioFormat::mono_16k(),
            Instant::now(),
        );
        assert_eq!(reference.played.len(), 0);
    }

    /// What is left of a speaker heard 30 ms late at half its level, from the third second on.
    /// Blocks go in turn rather than in real time, so the answer is the same on any machine.
    #[cfg(feature = "webrtc-aec")]
    fn echo_left(referenced: bool, speaker: AudioFormat, microphone: AudioFormat) -> f64 {
        let canceller: Box<dyn EchoCanceller> =
            Box::new(super::webrtc::WebrtcCanceller::new(16_000).unwrap());
        let reference = Arc::new(EchoReference::new());
        let mut stage = EchoStage::new(
            Arc::new(Mutex::new(canceller)),
            Arc::clone(&reference),
            microphone,
        )
        .unwrap();

        let rate = microphone.sample_rate as usize;
        let (block, delay) = (rate / 100, rate * 3 / 100);
        let mut seed = 3u32;
        let played: Vec<i16> = (0..rate * 4)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 16) as i16 / 2
            })
            .collect();

        let (mut energy, mut seen) = (0.0, 0);
        for (n, frames) in played.chunks_exact(block).enumerate() {
            if referenced {
                let channels = usize::from(speaker.channels);
                let spoken = frames
                    .iter()
                    .flat_map(|s| std::iter::repeat_n(*s, channels))
                    .collect();
                reference.push(Samples::I16(spoken), speaker, Instant::now());
            }
            let heard = (n * block..(n + 1) * block)
                .map(|i| i.checked_sub(delay).map_or(0, |j| played[j] / 2))
                .collect();
            let Samples::F32(cleaned) = stage.process(&Samples::I16(heard)).unwrap() else {
                unreachable!("the stage hands back f32");
            };
            for sample in cleaned {
                if seen >= 32_000 {
                    energy += f64::from(sample).powi(2);
                }
                seen += 1;
            }
        }
        energy
    }

    #[cfg(feature = "webrtc-aec")]
    #[test]
    fn webrtc_takes_the_echo_out_through_the_stage_and_its_resampling() {
        let stereo_48k = AudioFormat::new(48_000, 2, SampleType::I16);
        let mono_48k = AudioFormat::new(48_000, 1, SampleType::I16);
        let mono_16k = AudioFormat::mono_16k();
        for (speaker, microphone) in [(mono_16k, mono_16k), (stereo_48k, mono_48k)] {
            let left_in = echo_left(false, speaker, microphone);
            let taken_out = echo_left(true, speaker, microphone);
            let reduction_db = 10.0 * (left_in / taken_out.max(1e-12)).log10();
            assert!(
                reduction_db > 10.0,
                "{speaker:?}: only {reduction_db:.1} dB of the echo was taken out"
            );
        }
    }
}
