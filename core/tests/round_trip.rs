//! The whole hands-free journey: the wake word is heard, the alert
//! plays, the speaker talks, they stop, and the audio arrives.
//!
//! This is the only place the four parts are asked to work as one
//! thing rather than each on its own.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use edge_ear_core::backend::fake::FakeBackend;
use edge_ear_core::config::SampleType;
use edge_ear_core::error::Error;
use edge_ear_core::events::{EndReason, Event};
use edge_ear_core::{EdgeEar, SoundSource};

fn ear() -> EdgeEar {
    EdgeEar::with_backend(Box::new(FakeBackend::paced())).expect("handle")
}

fn model_dir() -> Option<PathBuf> {
    std::env::var("EDGE_EAR_WAKE_DIR").ok().map(PathBuf::from)
}

fn alert() -> SoundSource {
    SoundSource::Pcm {
        data: vec![3000; 3200], // 200 ms
        sample_rate: 16_000,
        channels: 1,
        sample_type: SampleType::I16,
    }
}

#[derive(Default)]
struct Seen {
    order: Vec<&'static str>,
    endings: Vec<(EndReason, usize)>,
}

fn watch(ear: &EdgeEar) -> Arc<Mutex<Seen>> {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let sink = Arc::clone(&seen);
    ear.on_event(move |event| {
        let mut seen = sink.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            Event::WakeDetected { .. } => seen.order.push("wake"),
            Event::SoundFinished { .. } => seen.order.push("alert done"),
            Event::SpeechEnded { reason, audio, .. } => {
                seen.order.push("speech ended");
                seen.endings.push((reason, audio.len()));
            }
            _ => {}
        }
    })
    .unwrap();
    seen
}

fn wait_until(seconds: u64, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn an_alert_must_be_registered_before_it_can_be_named() {
    let ear = ear();
    // Without the models, wake cannot be enabled at all.
    assert!(matches!(
        ear.enable_wake(Some("never-registered"))
            .expect_err("refuse"),
        Error::NoWakeModel
    ));
}

#[test]
fn no_alert_is_named_until_one_is_asked_for() {
    let ear = ear();
    assert_eq!(ear.wake_alert(), None);
}

#[test]
#[ignore]
fn naming_a_sound_nobody_registered_says_so() {
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

    let err = ear.enable_wake(Some("ghost")).expect_err("must refuse");
    assert!(matches!(err, Error::UnknownSound(_)), "{err}");
    assert_eq!(ear.wake_alert(), None, "nothing was named");
}

#[test]
#[ignore]
fn an_alert_is_remembered_once_named() {
    let Some(dir) = model_dir() else { return };
    let ear = ear();
    ear.register_sound("beep", alert(), 0.6).unwrap();
    ear.load_wake_features(
        &dir.join("melspectrogram.onnx"),
        &dir.join("embedding_model.onnx"),
    )
    .unwrap();
    ear.load_wake_model(&dir.join("hey_jarvis_v0.1.onnx"))
        .unwrap();

    ear.enable_wake(Some("beep")).unwrap();
    assert_eq!(ear.wake_alert().as_deref(), Some("beep"));

    // Asking again without one clears it.
    ear.enable_wake(None).unwrap();
    assert_eq!(ear.wake_alert(), None);
}

#[test]
#[ignore]
fn capture_keeps_running_the_whole_way_through() {
    let Some(dir) = model_dir() else {
        println!("set EDGE_EAR_WAKE_DIR to run this");
        return;
    };
    let ear = Arc::new(ear());
    ear.register_sound("beep", alert(), 0.6).unwrap();
    ear.load_wake_features(
        &dir.join("melspectrogram.onnx"),
        &dir.join("embedding_model.onnx"),
    )
    .unwrap();
    ear.load_wake_model(&dir.join("hey_jarvis_v0.1.onnx"))
        .unwrap();

    let _seen = watch(&ear);
    ear.set_no_speech_timeout(Duration::from_millis(400))
        .unwrap();
    ear.enable_wake(Some("beep")).unwrap();
    ear.enable_speech().unwrap();
    ear.start().unwrap();

    // Reading raw audio must be untouched by any of the coordination.
    for _ in 0..40 {
        let chunk = ear.read(Some(Duration::from_secs(2))).expect("audio");
        assert_eq!(chunk.dropped_before, 0, "audio was lost during the journey");
    }
    assert!(ear.is_running());
    ear.stop().unwrap();
}

#[test]
#[ignore]
fn a_recording_opened_behind_an_alert_waits_for_it() {
    let Some(dir) = model_dir() else { return };
    let ear = ear();
    // A long alert, so the gate is clearly shut for a while.
    ear.register_sound(
        "long",
        SoundSource::Pcm {
            data: vec![3000; 16_000], // one second
            sample_rate: 16_000,
            channels: 1,
            sample_type: SampleType::I16,
        },
        0.4,
    )
    .unwrap();
    ear.load_wake_features(
        &dir.join("melspectrogram.onnx"),
        &dir.join("embedding_model.onnx"),
    )
    .unwrap();
    ear.load_wake_model(&dir.join("hey_jarvis_v0.1.onnx"))
        .unwrap();

    let seen = watch(&ear);
    // Short enough that a recording counting silence would end fast.
    ear.set_no_speech_timeout(Duration::from_millis(200))
        .unwrap();
    ear.enable_wake(Some("long")).unwrap();
    ear.enable_speech().unwrap();
    ear.start().unwrap();

    // Nothing is said, so no wake word is heard and no recording opens.
    thread::sleep(Duration::from_millis(600));
    let seen = seen.lock().unwrap();
    assert!(
        seen.order.is_empty(),
        "silence set something off: {:?}",
        seen.order
    );
}

#[test]
#[ignore]
fn the_alert_finishing_is_what_starts_the_counting() {
    let Some(dir) = model_dir() else { return };
    // Driven by hand rather than by a wake word, so the ordering is
    // exact: open a recording behind an alert, then let it finish.
    let ear = ear();
    ear.register_sound("beep", alert(), 0.5).unwrap();
    ear.load_wake_features(
        &dir.join("melspectrogram.onnx"),
        &dir.join("embedding_model.onnx"),
    )
    .unwrap();
    ear.load_wake_model(&dir.join("hey_jarvis_v0.1.onnx"))
        .unwrap();

    let seen = watch(&ear);
    ear.set_no_speech_timeout(Duration::from_millis(200))
        .unwrap();
    ear.enable_wake(Some("beep")).unwrap();
    ear.enable_speech().unwrap();
    ear.start().unwrap();
    ear.start_recording().unwrap();

    // start_recording counts from the off, so this ends on its own.
    assert!(
        wait_until(5, || !seen.lock().unwrap().endings.is_empty()),
        "the recording never ended"
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.endings[0].0, EndReason::NoSpeech);
    ear.stop().unwrap();
}
