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
    sample_rate: u32,
}

/// Values the recurrent state holds: two layers, one stream, 128 wide.
const STATE_VALUES: usize = 2 * 128;

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
            sample_rate: format.sample_rate,
        })
    }
}

impl SpeechModel for SileroModel {
    fn probability(&mut self, frame: &[i16]) -> Result<f32> {
        let audio: Vec<f32> = frame.iter().map(|s| *s as f32 / 32768.0).collect();
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
    }
}

#[cfg(test)]
mod silero_tests {
    use super::*;

    /// Loads the bundled model, so it is slower than the rest and kept
    /// out of the normal run.
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
