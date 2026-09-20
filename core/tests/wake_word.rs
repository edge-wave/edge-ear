//! Being told when the wake word was said. The application supplies all
//! three models, so most of what is worth checking is what happens when
//! it supplies the wrong thing, or nothing.

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
    let err = ear.enable_wake(None).expect_err("must refuse");
    assert!(matches!(err, Error::NoWakeModel), "{err}");
    assert!(!ear.is_wake_enabled());
}

#[test]
fn a_wake_word_without_the_shared_models_says_what_is_missing() {
    let ear = ear();
    let err = ear
        .add_wake_model("anything", &PathBuf::from("anything.onnx"))
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
        ear.add_wake_model("c", &PathBuf::from("c.onnx")),
    ] {
        let err = result.expect_err("must refuse");
        assert!(matches!(err, Error::RunningNotAllowed { .. }), "{err}");
    }
}

#[test]
fn everything_else_keeps_working_without_a_wake_word() {
    let ear = ear();
    assert!(ear.enable_wake(None).is_err());

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
    ear.add_wake_model("hey_jarvis", &dir.join("hey_jarvis_v0.1.onnx"))
        .unwrap();

    // Accepted before capture starts, and in force once it does.
    ear.enable_wake(None).unwrap();
    assert!(ear.is_wake_enabled());
    ear.start().unwrap();
    assert!(ear.is_wake_enabled());

    ear.disable_wake().unwrap();
    assert!(!ear.is_wake_enabled());
    ear.enable_wake(None).unwrap();

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
    ear.add_wake_model("hey_jarvis", &dir.join("hey_jarvis_v0.1.onnx"))
        .unwrap();

    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    ear.on_event(move |event| {
        if let Event::WakeDetected { score, .. } = event {
            sink.lock().unwrap_or_else(|e| e.into_inner()).push(score);
        }
    })
    .unwrap();

    ear.enable_wake(None).unwrap();
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
        .add_wake_model("embedding", &dir.join("embedding_model.onnx"))
        .expect_err("must refuse");
    assert!(matches!(err, Error::ModelInvalid { .. }), "{err}");
    println!("{err}");
}

/// The name is handed to C as text later, where a control character
/// would cut it short or lose it altogether.
#[test]
fn a_name_that_would_not_survive_being_handed_to_c_is_refused() {
    let ear = ear();
    for name in ["", " ", "a\0b", "two\nlines", "\u{7}bell"] {
        let err = ear
            .add_wake_model(name, &PathBuf::from("anything.onnx"))
            .expect_err("must refuse");
        assert!(matches!(err, Error::InvalidValue { .. }), "{name:?}: {err}");
    }
    assert!(ear.wake_models().is_empty(), "a refused name was kept");
}

#[test]
fn a_score_is_asked_for_by_the_name_of_a_loaded_word() {
    let ear = ear();
    let err = ear.wake_score("jarvis").expect_err("nothing is loaded");
    assert!(matches!(err, Error::UnknownWakeWord(_)), "{err}");
}

#[test]
#[ignore]
fn scores_are_reported_even_when_they_fall_short() {
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
    ear.add_wake_model("hey_jarvis", &dir.join("hey_jarvis_v0.1.onnx"))
        .unwrap();
    ear.enable_wake(None).unwrap();
    ear.start().unwrap();

    // Silence never reaches the threshold, so without reporting the
    // ones that fall short there would be nothing to see at all.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut score = None;
    while score.is_none() && std::time::Instant::now() < deadline {
        score = ear.wake_score("hey_jarvis").unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }

    let score = score.expect("no score arrived");
    println!("silence scored {score:.4}");
    assert!((0.0..=1.0).contains(&score), "a score outside 0 to 1");
    assert!(score < 0.5, "silence read as the wake word");
    ear.stop().unwrap();
}
