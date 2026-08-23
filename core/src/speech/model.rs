//! Deciding whether a frame of audio is speech.
//!
//! The decision sits behind a trait for one reason: everything built on
//! top of it — recording, silence timing, pre-roll, why a recording
//! ended — is logic worth testing without a model in the way. A scripted
//! stand-in makes those tests exact and quick.
//!
//! Only one real model ships. The trait is not a way to run several.

use crate::config::AudioFormat;
use crate::error::{Error, Result};

/// Turns one frame of audio into how likely it is to be speech.
///
/// Implementations carry state between calls, so one belongs to one
/// recording and must be reset when a new recording opens.
pub trait SpeechModel: Send {
    /// How likely this frame is speech, from 0.0 to 1.0.
    fn probability(&mut self, frame: &[i16]) -> Result<f32>;

    /// Forget everything heard so far.
    ///
    /// A recording must start from nothing. State left over from the
    /// previous recording changes the decision on this one.
    fn reset(&mut self);
}

/// A stand-in that answers from a script instead of listening.
///
/// Tests use it to lay out speech and silence exactly, so what is being
/// checked is the timing and the decisions, not the model.
#[cfg(test)]
pub struct ScriptedModel {
    /// One probability per call, in order. Once used up, the last value
    /// repeats, so a test can end on speech or on silence and let it run.
    script: Vec<f32>,
    position: usize,
    /// Counts how often the model was told to forget, so a test can
    /// check a recording really did start from nothing.
    pub resets: usize,
}

#[cfg(test)]
impl ScriptedModel {
    pub fn new(script: Vec<f32>) -> Self {
        Self {
            script,
            position: 0,
            resets: 0,
        }
    }

    /// Speech for the given number of frames, then silence for ever.
    pub fn speech_then_silence(speech_frames: usize) -> Self {
        let mut script = vec![0.9; speech_frames];
        script.push(0.0);
        Self::new(script)
    }

    /// Never hears anything.
    pub fn silent() -> Self {
        Self::new(vec![0.0])
    }

    /// Always hears speech, so a recording only ends on a limit.
    pub fn always_speaking() -> Self {
        Self::new(vec![0.9])
    }
}

#[cfg(test)]
impl SpeechModel for ScriptedModel {
    fn probability(&mut self, _frame: &[i16]) -> Result<f32> {
        let value = self
            .script
            .get(self.position)
            .copied()
            .or_else(|| self.script.last().copied())
            .unwrap_or(0.0);
        self.position += 1;
        Ok(value)
    }

    fn reset(&mut self) {
        self.position = 0;
        self.resets += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_is_followed_in_order() {
        let mut model = ScriptedModel::new(vec![0.1, 0.9, 0.2]);
        assert_eq!(model.probability(&[]).unwrap(), 0.1);
        assert_eq!(model.probability(&[]).unwrap(), 0.9);
        assert_eq!(model.probability(&[]).unwrap(), 0.2);
    }

    #[test]
    fn the_last_value_repeats_once_the_script_runs_out() {
        let mut model = ScriptedModel::speech_then_silence(2);
        assert_eq!(model.probability(&[]).unwrap(), 0.9);
        assert_eq!(model.probability(&[]).unwrap(), 0.9);
        for _ in 0..100 {
            assert_eq!(model.probability(&[]).unwrap(), 0.0);
        }
    }

    #[test]
    fn a_reset_starts_the_script_again_and_is_counted() {
        let mut model = ScriptedModel::new(vec![0.1, 0.9]);
        model.probability(&[]).unwrap();
        model.reset();
        assert_eq!(model.resets, 1);
        assert_eq!(model.probability(&[]).unwrap(), 0.1);
    }
}

/// The model this library ships, run through ONNX Runtime.
///
/// Its interface is fixed by the model file: one window of audio, a
/// recurrent state carried between calls, and the sample rate. Model
/// interfaces drift between versions, so a copy from anywhere but the
/// project that publishes it may not match.
pub struct SileroModel {
    session: ort::session::Session,
    /// Shape [2, 1, 128], carried from one call to the next.
    state: Vec<f32>,
    /// The tail of the last window, put in front of the next one.
    ///
    /// The model is handed context plus frame, not the frame alone. Its
    /// input length is not fixed in the file, so leaving the context off
    /// is accepted without complaint and quietly answers nothing.
    context: Vec<f32>,
    sample_rate: u32,
}

/// Values the recurrent state holds: two layers, one stream, 128 wide.
const STATE_VALUES: usize = 2 * 128;

/// Samples of the previous window handed back to the model.
fn context_samples(sample_rate: u32) -> usize {
    if sample_rate == 8_000 { 32 } else { 64 }
}

impl SileroModel {
    /// Load the model that ships with this library.
    pub fn bundled(format: AudioFormat) -> Result<Self> {
        const BYTES: &[u8] = include_bytes!("../../assets/silero_vad.onnx");
        let session = ort::session::Session::builder()
            .and_then(|mut b| b.commit_from_memory(BYTES))
            .map_err(|e| Error::ModelInvalid {
                path: "the bundled speech model".into(),
                reason: e.to_string(),
            })?;
        Ok(Self {
            session,
            state: vec![0.0; STATE_VALUES],
            context: vec![0.0; context_samples(format.sample_rate)],
            sample_rate: format.sample_rate,
        })
    }
}

impl SpeechModel for SileroModel {
    fn probability(&mut self, frame: &[i16]) -> Result<f32> {
        // Context first, then the new frame. The model reads both and
        // answers about the frame.
        let mut audio: Vec<f32> = Vec::with_capacity(self.context.len() + frame.len());
        audio.extend_from_slice(&self.context);
        audio.extend(frame.iter().map(|s| *s as f32 / 32768.0));

        let keep = context_samples(self.sample_rate);
        self.context = audio[audio.len().saturating_sub(keep)..].to_vec();

        let samples = audio.len() as i64;

        let build = |what: &'static str, shape: Vec<i64>, data: Vec<f32>| {
            ort::value::Tensor::from_array((shape, data)).map_err(|e| Error::Conversion {
                reason: format!("{what} could not be handed to the model: {e}"),
            })
        };

        let input = build("audio", vec![1, samples], audio)?;
        let state = build("state", vec![2, 1, 128], self.state.clone())?;
        let rate =
            ort::value::Tensor::from_array((Vec::<i64>::new(), vec![self.sample_rate as i64]))
                .map_err(|e| Error::Conversion {
                    reason: format!("the sample rate could not be handed to the model: {e}"),
                })?;

        let outputs = self
            .session
            .run(ort::inputs!["input" => input, "state" => state, "sr" => rate])
            .map_err(|e| Error::Conversion {
                reason: format!("speech detection failed: {e}"),
            })?;

        let (_, next) =
            outputs["stateN"]
                .try_extract_tensor::<f32>()
                .map_err(|e| Error::Conversion {
                    reason: format!("the model returned no usable state: {e}"),
                })?;
        self.state = next.to_vec();

        let (_, probability) =
            outputs["output"]
                .try_extract_tensor::<f32>()
                .map_err(|e| Error::Conversion {
                    reason: format!("the model returned no usable answer: {e}"),
                })?;
        Ok(probability.first().copied().unwrap_or(0.0))
    }

    fn reset(&mut self) {
        // Zeroing is the whole reset. Leaving it is a real defect, not a
        // tidiness one: what the last recording heard would carry into
        // this one and change the answer.
        self.state = vec![0.0; STATE_VALUES];
        self.context = vec![0.0; context_samples(self.sample_rate)];
    }
}

#[cfg(test)]
mod silero_tests {
    use super::*;

    /// Loads the bundled model, so it is slower than the rest and kept
    /// out of the normal run.
    /// The shape of the bundled model, checked directly.
    ///
    /// Other projects ship a model under a similar name with a
    /// different shape: two state tensors instead of one, wider
    /// windows, and no sample rate input. Swapping one in would break
    /// this library in ways the behaviour tests would not name. This
    /// says exactly what the file must look like.
    #[test]
    #[ignore]
    fn the_bundled_model_has_the_shape_this_code_expects() {
        let session = ort::session::Session::builder()
            .and_then(|mut b| b.commit_from_memory(include_bytes!("../../assets/silero_vad.onnx")))
            .expect("the bundled model");

        let inputs: Vec<&str> = session.inputs().iter().map(|o| o.name()).collect();
        let outputs: Vec<&str> = session.outputs().iter().map(|o| o.name()).collect();
        println!("inputs {inputs:?}, outputs {outputs:?}");

        assert_eq!(
            inputs,
            vec!["input", "state", "sr"],
            "one state tensor and a sample rate input, not a split state"
        );
        assert_eq!(outputs, vec!["output", "stateN"]);
    }

    #[test]
    #[ignore]
    fn the_bundled_model_loads_and_answers() {
        let mut model = SileroModel::bundled(AudioFormat::mono_16k()).expect("the bundled model");

        let silence = vec![0i16; 512];
        let quiet = model.probability(&silence).expect("an answer");
        assert!(
            (0.0..=1.0).contains(&quiet),
            "a probability must be between 0 and 1, got {quiet}"
        );
        assert!(quiet < 0.5, "silence should not read as speech: {quiet}");
        println!("silence -> {quiet:.4}");
    }

    #[test]
    #[ignore]
    fn the_answer_changes_with_what_it_hears() {
        let mut model = SileroModel::bundled(AudioFormat::mono_16k()).unwrap();
        let silence = model.probability(&vec![0i16; 512]).unwrap();

        // Noise is not speech either, but it must not give the identical
        // number, or the model is not really looking at the audio.
        let noise: Vec<i16> = (0..512)
            .map(|n| (((n * 7919) % 4096) as i16) - 2048)
            .collect();
        let noisy = model.probability(&noise).unwrap();
        println!("silence -> {silence:.4}, noise -> {noisy:.4}");
        assert!(
            (silence - noisy).abs() > f32::EPSILON,
            "the model gave the same answer for silence and noise"
        );
    }

    /// The model is handed the tail of the previous window along with
    /// the new one. Leaving that off is accepted without complaint and
    /// makes the model answer nothing to everything, which is how this
    /// went unnoticed once already.
    #[test]
    #[ignore]
    fn the_tail_of_each_window_is_carried_into_the_next() {
        let mut model = SileroModel::bundled(AudioFormat::mono_16k()).unwrap();
        assert_eq!(model.context.len(), 64, "one window of context at 16 kHz");
        assert!(
            model.context.iter().all(|s| *s == 0.0),
            "a fresh model starts with nothing behind it"
        );

        let loud: Vec<i16> = (0..512).map(|n| ((n * 37) % 8000) as i16 - 4000).collect();
        model.probability(&loud).unwrap();

        assert_eq!(model.context.len(), 64, "the context stays one window wide");
        assert!(
            model.context.iter().any(|s| *s != 0.0),
            "the tail of the window was not kept"
        );

        // The tail kept must be the end of what was just heard.
        let expected: Vec<f32> = loud[512 - 64..]
            .iter()
            .map(|s| *s as f32 / 32768.0)
            .collect();
        assert_eq!(model.context, expected);

        // And a reset clears it, so a new recording starts from nothing.
        model.reset();
        assert!(model.context.iter().all(|s| *s == 0.0));
    }

    /// What the model is given must be context plus frame, not the
    /// frame alone. The file does not fix its own input length, so a
    /// frame on its own is accepted and answers nothing.
    #[test]
    #[ignore]
    fn the_model_is_given_more_than_the_frame() {
        let mut model = SileroModel::bundled(AudioFormat::mono_16k()).unwrap();
        let frame = vec![1000i16; 512];
        model.probability(&frame).unwrap();
        // 64 of context sat in front of the 512 that went in.
        assert_eq!(model.context.len() + 512, 576);
    }

    #[test]
    #[ignore]
    fn a_reset_puts_the_model_back_where_it_started() {
        let mut model = SileroModel::bundled(AudioFormat::mono_16k()).unwrap();
        let noise: Vec<i16> = (0..512).map(|n| ((n * 31) % 2000) as i16).collect();

        let first = model.probability(&noise).unwrap();
        // Feed more, so the state is no longer where it began.
        for _ in 0..5 {
            model.probability(&noise).unwrap();
        }
        model.reset();
        let after_reset = model.probability(&noise).unwrap();

        assert_eq!(
            first, after_reset,
            "the same audio from a fresh state must give the same answer"
        );
    }
}

/// Diagnostics for when the library says one thing and your ears say
/// another. Point them at a recording and see what the model decides.
///
///     EDGE_EAR_WAV=/path/to/speech.wav \
///       cargo test -p edge-ear-core --lib what_the_model_hears -- --ignored --nocapture
#[cfg(test)]
mod diagnostics {
    use super::*;
    use crate::capture::Samples;
    use crate::capture::convert::Converter;
    use crate::config::SampleType;

    #[test]
    #[ignore]
    fn what_the_model_hears() {
        let Ok(path) = std::env::var("EDGE_EAR_WAV") else {
            println!("set EDGE_EAR_WAV to a wav file to run this");
            return;
        };

        let (raw, spec) =
            crate::player::registry::decode_file(&path.clone().into()).expect("read the wav");
        let peak = raw.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        println!("{path}");
        println!(
            "  {} Hz, {} ch, {} samples, peak {peak}",
            spec.sample_rate,
            spec.channels,
            raw.len()
        );

        let to = AudioFormat::mono_16k();
        let audio = if spec.sample_rate == 16_000 && spec.channels == 1 {
            raw
        } else {
            let from = AudioFormat::new(spec.sample_rate, spec.channels, SampleType::I16);
            let mut converter = Converter::new(from, to).expect("a converter");
            let mut out = match converter.convert(&Samples::I16(raw)).expect("convert") {
                Samples::I16(v) => v,
                Samples::F32(v) => v.iter().map(|s| (s * 32767.0) as i16).collect(),
            };
            if let Ok(Samples::I16(mut tail)) = converter.flush() {
                out.append(&mut tail);
            }
            out
        };
        println!("  {} samples at 16 kHz mono", audio.len());

        let mut model = SileroModel::bundled(to).expect("the bundled model");
        let (mut best, mut over, mut frames) = (0.0f32, 0usize, 0usize);
        let mut marks = String::new();

        for frame in audio.chunks(512) {
            if frame.len() < 512 {
                break;
            }
            let p = model.probability(frame).expect("an answer");
            best = best.max(p);
            frames += 1;
            if p >= 0.5 {
                over += 1;
            }
            if marks.len() < 100 {
                marks.push(match p {
                    p if p >= 0.9 => '#',
                    p if p >= 0.5 => '+',
                    p if p >= 0.2 => '-',
                    _ => '.',
                });
            }
        }

        println!("  {frames} frames, highest {best:.3}, {over} over the 0.5 threshold");
        println!("  [{marks}]");
        println!("  # over 0.9   + over 0.5   - over 0.2   . quiet");
    }
}

/// The same question asked of the whole library rather than the model
/// alone: does real speech end a recording by going quiet?
#[cfg(test)]
mod pipeline_diagnostics {
    use crate::backend::fake::FakeBackend;
    use crate::config::{AudioFormat, SampleType};
    use crate::events::{EndReason, Event};
    use crate::{EdgeEar, player};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    #[test]
    #[ignore]
    fn real_speech_ends_a_recording_by_going_quiet() {
        let Ok(path) = std::env::var("EDGE_EAR_WAV") else {
            println!("set EDGE_EAR_WAV to a wav of speech to run this");
            return;
        };

        let (audio, spec) =
            player::registry::decode_file(&path.clone().into()).expect("read the wav");
        println!(
            "{path}: {} Hz, {} ch, {} samples",
            spec.sample_rate,
            spec.channels,
            audio.len()
        );

        let device = AudioFormat::new(spec.sample_rate, spec.channels, SampleType::I16);
        let block = spec.sample_rate as usize / 50;
        let ear = EdgeEar::with_backend(Box::new(FakeBackend::playing(audio, device, block)))
            .expect("handle");

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        ear.on_event(move |event| {
            if let Event::SpeechEnded { reason, audio, .. } = event {
                sink.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((reason, audio.len()));
            }
        })
        .unwrap();

        ear.set_silence_duration(Duration::from_millis(600))
            .unwrap();
        ear.set_no_speech_timeout(Duration::from_secs(30)).unwrap();
        ear.set_max_recording(Duration::from_secs(60)).unwrap();
        ear.enable_speech().unwrap();
        ear.start().unwrap();
        ear.start_recording().unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        while seen.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let seen = seen.lock().unwrap();
        let (reason, samples) = *seen.first().expect("the recording never ended");
        println!("ended: {reason} with {samples} samples");
        assert_eq!(
            reason,
            EndReason::Silence,
            "speech should end a recording by stopping, not by timing out"
        );
    }
}
