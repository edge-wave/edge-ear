//! Spotting a wake word, in three stages.
//!
//! Audio becomes a mel spectrogram, the spectrogram becomes general
//! speech features, and a small model decides whether those features
//! are the phrase it was trained on. Only the last one knows the word,
//! and it is the only one the application supplies.
//!
//! Tensor names are never part of the contract. Every model exported
//! from the training pipeline has the same shapes but its own
//! auto-generated names, so a name in this file would work for one
//! wake word and fail for the next. Names are read from each file when
//! it is loaded.

use std::path::{Path, PathBuf};

use ort::session::Session;
use ort::value::Tensor;

use crate::error::{Error, Result};

/// Audio the pipeline takes at a time: 80 ms at 16 kHz.
pub const FRAME_SAMPLES: usize = 1280;
/// Extra audio fed to the spectrogram so the new frames come out right.
/// Three hops of the 10 ms step.
const SPECTROGRAM_CONTEXT: usize = 160 * 3;
/// Mel frames the feature model looks at. One frame of audio produces
/// eight of them, so a new set of features comes out every frame once
/// the window has filled.
const MEL_WINDOW: usize = 76;
/// Values in one set of speech features.
const FEATURE_WIDTH: usize = 96;
/// Sets of features the wake word model looks at.
const FEATURE_WINDOW: usize = 16;

/// Brings the spectrogram nearer the original this model was copied
/// from. Leaving it out changes every answer and reports no error.
fn adjust(value: f32) -> f32 {
    value / 10.0 + 2.0
}

/// A loaded model, with the names its own file happens to use.
struct Stage {
    session: Session,
    input: String,
    output: String,
    path: PathBuf,
}

impl Stage {
    /// Load a model and take its names from the file rather than
    /// assuming any.
    fn load(path: &Path, what: &'static str) -> Result<Self> {
        if !path.exists() {
            return Err(Error::ModelNotFound {
                path: path.to_path_buf(),
            });
        }
        let session = Session::builder()
            .and_then(|mut b| b.commit_from_file(path))
            .map_err(|e| Error::ModelInvalid {
                path: path.to_path_buf(),
                reason: format!("not a model this library can read: {e}"),
            })?;

        let inputs = session.inputs();
        let outputs = session.outputs();
        if inputs.len() != 1 || outputs.len() != 1 {
            return Err(Error::ModelInvalid {
                path: path.to_path_buf(),
                reason: format!(
                    "the {what} model must take one input and give one output, \
                     this one takes {} and gives {}",
                    inputs.len(),
                    outputs.len()
                ),
            });
        }

        Ok(Self {
            input: inputs[0].name().to_string(),
            output: outputs[0].name().to_string(),
            session,
            path: path.to_path_buf(),
        })
    }

    fn run(&mut self, shape: Vec<i64>, data: Vec<f32>) -> Result<(Vec<i64>, Vec<f32>)> {
        let tensor = Tensor::from_array((shape, data)).map_err(|e| Error::Conversion {
            reason: format!("audio could not be handed to {}: {e}", self.path.display()),
        })?;
        let outputs = self
            .session
            .run(ort::inputs![self.input.as_str() => tensor])
            .map_err(|e| Error::Conversion {
                reason: format!("{} failed: {e}", self.path.display()),
            })?;
        let (shape, values) = outputs[self.output.as_str()]
            .try_extract_tensor::<f32>()
            .map_err(|e| Error::Conversion {
                reason: format!("{} returned nothing usable: {e}", self.path.display()),
            })?;
        Ok((shape.iter().copied().collect(), values.to_vec()))
    }
}

/// Turning frames of audio into how likely the wake word just finished.
///
/// Behind a trait for one reason: everything built on top of it — when
/// a score counts, how long to look away afterwards, what happens the
/// moment it counts — is logic worth testing without three model files
/// in the way. A scripted stand-in makes those tests exact and quick.
///
/// Only one real source ships. The trait is not a way to run several.
pub trait WakeSource: Send {
    /// Feed one frame of audio. Gives a score once enough has been
    /// heard to give one.
    fn push(&mut self, frame: &[i16]) -> Result<Option<f32>>;

    /// Forget everything heard so far.
    fn reset(&mut self);
}

/// The three stages together, with the buffers between them.
pub struct WakeModel {
    spectrogram: Stage,
    features: Stage,
    /// The wake word itself. Absent until an application supplies one.
    word: Option<Stage>,
    /// Audio kept so the next spectrogram call has its lead-in.
    audio_tail: Vec<f32>,
    mel: Vec<f32>,
    speech: Vec<f32>,
}

impl WakeModel {
    /// Load the two stages every wake word shares. Neither knows any
    /// word; both are the same whatever the application is listening
    /// for.
    pub fn new(spectrogram: &Path, features: &Path) -> Result<Self> {
        Ok(Self {
            spectrogram: Stage::load(spectrogram, "spectrogram")?,
            features: Stage::load(features, "feature")?,
            word: None,
            audio_tail: vec![0.0; SPECTROGRAM_CONTEXT],
            mel: Vec::new(),
            speech: Vec::new(),
        })
    }

    /// Load the model for the phrase to listen for.
    ///
    /// Its shape is checked here rather than left to fail oddly later.
    /// A model built for a different pipeline is refused by name of
    /// what was wrong.
    pub fn load_word(&mut self, path: &Path) -> Result<()> {
        let stage = Stage::load(path, "wake word")?;

        let expected = [1i64, FEATURE_WINDOW as i64, FEATURE_WIDTH as i64];
        let shape = shape_of(&stage.session.inputs()[0]);
        if shape != expected {
            return Err(Error::ModelInvalid {
                path: path.to_path_buf(),
                reason: format!(
                    "a wake word model must take {expected:?}, this one takes {shape:?}"
                ),
            });
        }
        let out = shape_of(&stage.session.outputs()[0]);
        if out != [1, 1] {
            return Err(Error::ModelInvalid {
                path: path.to_path_buf(),
                reason: format!("a wake word model must give [1, 1], this one gives {out:?}"),
            });
        }

        self.word = Some(stage);
        self.clear();
        Ok(())
    }

    #[cfg(test)]
    pub fn has_word(&self) -> bool {
        self.word.is_some()
    }

    fn feed(&mut self, frame: &[i16]) -> Result<Option<f32>> {
        if self.word.is_none() {
            return Err(Error::NoWakeModel);
        }
        if frame.len() != FRAME_SAMPLES {
            return Err(Error::Conversion {
                reason: format!(
                    "the wake word pipeline takes {FRAME_SAMPLES} samples at a time, \
                     not {}",
                    frame.len()
                ),
            });
        }

        self.add_spectrogram(frame)?;
        if !self.add_features()? {
            return Ok(None);
        }
        self.score()
    }

    fn clear(&mut self) {
        self.audio_tail = vec![0.0; SPECTROGRAM_CONTEXT];
        self.mel.clear();
        self.speech.clear();
    }

    fn add_spectrogram(&mut self, frame: &[i16]) -> Result<()> {
        let mut audio = Vec::with_capacity(SPECTROGRAM_CONTEXT + frame.len());
        audio.extend_from_slice(&self.audio_tail);
        audio.extend(frame.iter().map(|s| *s as f32));
        self.audio_tail = audio[audio.len() - SPECTROGRAM_CONTEXT..].to_vec();

        let samples = audio.len() as i64;
        let (_, values) = self.spectrogram.run(vec![1, samples], audio)?;
        self.mel.extend(values.iter().map(|v| adjust(*v)));

        // Keep a few windows of history and no more.
        let keep = MEL_WINDOW * 4 * 32;
        if self.mel.len() > keep {
            self.mel.drain(..self.mel.len() - keep);
        }
        Ok(())
    }

    /// Turn the newest stretch of spectrogram into one set of features.
    /// Returns false while there is not yet a full window.
    fn add_features(&mut self) -> Result<bool> {
        let window = MEL_WINDOW * 32;
        if self.mel.len() < window {
            return Ok(false);
        }
        let recent = self.mel[self.mel.len() - window..].to_vec();
        let (_, values) = self
            .features
            .run(vec![1, MEL_WINDOW as i64, 32, 1], recent)?;
        self.speech.extend_from_slice(&values);

        let keep = FEATURE_WINDOW * 8 * FEATURE_WIDTH;
        if self.speech.len() > keep {
            self.speech.drain(..self.speech.len() - keep);
        }
        Ok(true)
    }

    fn score(&mut self) -> Result<Option<f32>> {
        let window = FEATURE_WINDOW * FEATURE_WIDTH;
        if self.speech.len() < window {
            return Ok(None);
        }
        let recent = self.speech[self.speech.len() - window..].to_vec();
        let word = self.word.as_mut().expect("checked by the caller");
        let (_, values) = word.run(vec![1, FEATURE_WINDOW as i64, FEATURE_WIDTH as i64], recent)?;
        Ok(values.first().copied())
    }
}

impl WakeSource for WakeModel {
    fn push(&mut self, frame: &[i16]) -> Result<Option<f32>> {
        self.feed(frame)
    }

    /// Clearing the score alone is not enough: the audio that caused a
    /// detection is still inside these buffers and would cause another
    /// one at once.
    fn reset(&mut self) {
        self.clear();
    }
}

fn shape_of(outlet: &ort::value::Outlet) -> Vec<i64> {
    match outlet.dtype() {
        ort::value::ValueType::Tensor { shape, .. } => shape.iter().copied().collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where the models are, for the tests that need real ones.
    ///
    ///     EDGE_EAR_WAKE_DIR=/path/to/models \
    ///       cargo test -p edge-ear-core --lib wake -- --ignored --nocapture
    fn model_dir() -> Option<PathBuf> {
        std::env::var("EDGE_EAR_WAKE_DIR").ok().map(PathBuf::from)
    }

    fn shared(dir: &Path) -> Result<WakeModel> {
        WakeModel::new(
            &dir.join("melspectrogram.onnx"),
            &dir.join("embedding_model.onnx"),
        )
    }

    #[test]
    #[ignore]
    fn the_shared_models_load_whatever_their_tensors_are_called() {
        let Some(dir) = model_dir() else {
            println!("set EDGE_EAR_WAKE_DIR to run this");
            return;
        };
        let model = shared(&dir).expect("the shared models");
        println!(
            "spectrogram: {} -> {}",
            model.spectrogram.input, model.spectrogram.output
        );
        println!(
            "features:    {} -> {}",
            model.features.input, model.features.output
        );
        assert!(!model.has_word(), "no wake word until one is supplied");
    }

    /// The names differ from one wake word to the next, so a name in
    /// the code would work for one and fail for the rest.
    #[test]
    #[ignore]
    fn every_wake_word_loads_despite_its_own_tensor_names() {
        let Some(dir) = model_dir() else {
            println!("set EDGE_EAR_WAKE_DIR to run this");
            return;
        };

        let mut names = Vec::new();
        for word in [
            "hey_jarvis_v0.1.onnx",
            "alexa_v0.1.onnx",
            "hey_mycroft_v0.1.onnx",
            "hey_rhasspy_v0.1.onnx",
        ] {
            let path = dir.join(word);
            if !path.exists() {
                continue;
            }
            let mut model = shared(&dir).unwrap();
            model
                .load_word(&path)
                .unwrap_or_else(|e| panic!("{word}: {e}"));
            let stage = model.word.as_ref().unwrap();
            println!("{word:<24} {} -> {}", stage.input, stage.output);
            names.push((stage.input.clone(), stage.output.clone()));
        }

        assert!(names.len() > 1, "needs more than one wake word to compare");
        assert!(
            names.iter().any(|n| *n != names[0]),
            "these all had the same names, so this proved nothing"
        );
    }

    #[test]
    #[ignore]
    fn a_model_of_the_wrong_shape_is_refused_by_name_of_what_is_wrong() {
        let Some(dir) = model_dir() else {
            println!("set EDGE_EAR_WAKE_DIR to run this");
            return;
        };
        let mut model = shared(&dir).unwrap();

        // The spectrogram model is a model, but not a wake word one.
        let err = model
            .load_word(&dir.join("melspectrogram.onnx"))
            .expect_err("must be refused");
        println!("{err}");
        assert!(matches!(err, Error::ModelInvalid { .. }), "{err}");
        assert!(
            err.to_string().contains("wake word model must take"),
            "{err}"
        );
    }

    #[test]
    #[ignore]
    fn a_missing_model_is_reported_as_missing() {
        let Some(dir) = model_dir() else { return };
        let mut model = shared(&dir).unwrap();
        let err = model
            .load_word(&dir.join("no_such_word.onnx"))
            .expect_err("must be refused");
        assert!(matches!(err, Error::ModelNotFound { .. }), "{err}");
    }

    #[test]
    #[ignore]
    fn feeding_audio_gives_a_score_once_enough_has_been_heard() {
        let Some(dir) = model_dir() else {
            println!("set EDGE_EAR_WAKE_DIR to run this");
            return;
        };
        let mut model = shared(&dir).unwrap();
        model.load_word(&dir.join("hey_jarvis_v0.1.onnx")).unwrap();

        let quiet = vec![0i16; FRAME_SAMPLES];
        let mut first_answer = None;
        let mut scores = Vec::new();
        for n in 0..40 {
            if let Some(score) = model.push(&quiet).unwrap() {
                first_answer.get_or_insert(n);
                scores.push(score);
            }
        }

        let at = first_answer.expect("the pipeline never answered");
        println!("first answer after {at} frames, {} scores", scores.len());
        assert!(at <= 30, "took {at} frames to say anything");
        assert!(
            scores.iter().all(|s| (0.0..=1.0).contains(s)),
            "a score outside 0 to 1"
        );
        assert!(
            scores.iter().all(|s| *s < 0.5),
            "silence read as the wake word: {:?}",
            scores.iter().take(5).collect::<Vec<_>>()
        );
    }

    #[test]
    #[ignore]
    fn pushing_without_a_wake_word_says_one_is_needed() {
        let Some(dir) = model_dir() else { return };
        let mut model = shared(&dir).unwrap();
        let err = model
            .push(&vec![0i16; FRAME_SAMPLES])
            .expect_err("must refuse");
        assert!(matches!(err, Error::NoWakeModel), "{err}");
    }

    #[test]
    #[ignore]
    fn a_frame_of_the_wrong_length_is_refused() {
        let Some(dir) = model_dir() else { return };
        let mut model = shared(&dir).unwrap();
        model.load_word(&dir.join("hey_jarvis_v0.1.onnx")).unwrap();
        let err = model.push(&vec![0i16; 512]).expect_err("must refuse");
        assert!(err.to_string().contains("1280"), "{err}");
    }
}
