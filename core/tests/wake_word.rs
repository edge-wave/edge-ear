//! Being told when the wake word was said.
//!
//! The library ships no wake word and no way to make one. An
//! application supplies all three models, so most of what is worth
//! checking here is what happens when it supplies the wrong thing, or
//! nothing at all.
//!
//! The tests that need real models read their location from
//! `EDGE_EAR_WAKE_DIR` and are left out of the normal run.

use std::path::PathBuf;
use std::time::Duration;

use edge_ear_core::EdgeEar;
use edge_ear_core::backend::fake::FakeBackend;
use edge_ear_core::error::Error;

fn ear() -> EdgeEar {
    EdgeEar::with_backend(Box::new(FakeBackend::paced())).expect("handle")
}

fn model_dir() -> Option<PathBuf> {
    std::env::var("EDGE_EAR_WAKE_DIR").ok().map(PathBuf::from)
}

#[test]
fn listening_without_a_model_says_one_is_needed() {
    let ear = ear();
    let err = ear.enable_wake().expect_err("must refuse");
    assert!(matches!(err, Error::NoWakeModel), "{err}");
    assert!(!ear.is_wake_enabled());
}

#[test]
fn a_wake_word_without_the_shared_models_says_what_is_missing() {
    let ear = ear();
    let err = ear
        .load_wake_model(&PathBuf::from("anything.onnx"))
        .expect_err("must refuse");
    assert!(matches!(err, Error::NoWakeModel), "{err}");
}

#[test]
fn a_missing_shared_model_is_reported_as_missing() {
    let ear = ear();
    let err = ear
        .load_wake_features(
            &PathBuf::from("/no/such/melspectrogram.onnx"),
            &PathBuf::from("/no/such/embedding_model.onnx"),
        )
        .expect_err("must refuse");
    assert!(matches!(err, Error::ModelNotFound { .. }), "{err}");
}

#[test]
fn models_cannot_be_swapped_while_capture_runs() {
    let ear = ear();
    ear.start().unwrap();

    for result in [
        ear.load_wake_features(&PathBuf::from("a.onnx"), &PathBuf::from("b.onnx")),
        ear.load_wake_model(&PathBuf::from("c.onnx")),
    ] {
        let err = result.expect_err("must refuse");
        assert!(matches!(err, Error::RunningNotAllowed { .. }), "{err}");
    }
}

#[test]
fn everything_else_keeps_working_without_a_wake_word() {
    let ear = ear();
    assert!(ear.enable_wake().is_err());

    // Reading raw audio and detecting speech are untouched by there
    // being no wake word.
    ear.enable_speech().unwrap();
    ear.start().unwrap();
    ear.read(Some(Duration::from_secs(2))).expect("audio");
    assert!(ear.is_speech_enabled());
    ear.stop().unwrap();
}

#[test]
fn the_threshold_can_be_set_before_and_after_start() {
    let ear = ear();
    ear.set_wake_threshold(0.8).unwrap();
    ear.start().unwrap();
    ear.set_wake_threshold(0.3).unwrap();
    assert_eq!(ear.config().tunable.wake_threshold, 0.3);

    let err = ear.set_wake_threshold(5.0).expect_err("must refuse");
    assert!(matches!(err, Error::InvalidValue { .. }), "{err}");
    assert_eq!(ear.config().tunable.wake_threshold, 0.3, "old value stands");
}

#[test]
#[ignore]
fn a_wake_word_can_be_switched_on_and_off_around_a_run() {
    let Some(dir) = model_dir() else {
        println!("set EDGE_EAR_WAKE_DIR to run this");
        return;
    };
    let ear = ear();
    ear.load_wake_features(
        &dir.join("melspectrogram.onnx"),
        &dir.join("embedding_model.onnx"),
    )
    .unwrap();
    ear.load_wake_model(&dir.join("hey_jarvis_v0.1.onnx"))
        .unwrap();

    // Accepted before capture starts, and in force once it does.
    ear.enable_wake().unwrap();
    assert!(ear.is_wake_enabled());
    ear.start().unwrap();
    assert!(ear.is_wake_enabled());

    ear.disable_wake().unwrap();
    assert!(!ear.is_wake_enabled());
    ear.enable_wake().unwrap();

    // Reading raw audio is untouched throughout.
    ear.read(Some(Duration::from_secs(2))).expect("audio");
    ear.stop().unwrap();
}

#[test]
#[ignore]
fn silence_is_never_reported_as_the_wake_word() {
    let Some(dir) = model_dir() else { return };
    use edge_ear_core::events::Event;
    use std::sync::{Arc, Mutex};

    let ear = ear();
    ear.load_wake_features(
        &dir.join("melspectrogram.onnx"),
        &dir.join("embedding_model.onnx"),
    )
    .unwrap();
    ear.load_wake_model(&dir.join("hey_jarvis_v0.1.onnx"))
        .unwrap();

    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    ear.on_event(move |event| {
        if let Event::WakeDetected { score } = event {
            sink.lock().unwrap_or_else(|e| e.into_inner()).push(score);
        }
    })
    .unwrap();

    ear.enable_wake().unwrap();
    ear.start().unwrap();
    std::thread::sleep(Duration::from_secs(3));
    ear.stop().unwrap();

    let heard = heard.lock().unwrap();
    assert!(
        heard.is_empty(),
        "silence was reported as speech: {heard:?}"
    );
}

#[test]
#[ignore]
fn a_model_that_is_not_a_wake_word_is_refused() {
    let Some(dir) = model_dir() else { return };
    let ear = ear();
    ear.load_wake_features(
        &dir.join("melspectrogram.onnx"),
        &dir.join("embedding_model.onnx"),
    )
    .unwrap();

    let err = ear
        .load_wake_model(&dir.join("embedding_model.onnx"))
        .expect_err("must refuse");
    assert!(matches!(err, Error::ModelInvalid { .. }), "{err}");
    println!("{err}");
}
