//! Capturing a spoken request without deciding when it ended. The
//! application opens a recording and is handed the audio once the
//! speaker goes quiet, with the reason it ended.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use edge_ear_core::EdgeEar;
use edge_ear_core::backend::fake::FakeBackend;
use edge_ear_core::config::AudioFormat;
use edge_ear_core::error::Error;
use edge_ear_core::events::{EndReason, Event};

/// Silence, so the detector reaches its no-speech timeout quickly.
fn ear() -> EdgeEar {
    EdgeEar::with_backend(Box::new(FakeBackend::playing(
        vec![0i16; 16_000 * 4],
        AudioFormat::mono_16k(),
        512,
    )))
    .expect("handle")
}

fn endings(ear: &EdgeEar) -> Arc<Mutex<Vec<(EndReason, usize)>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    ear.on_event(move |event| {
        if let Event::SpeechEnded { audio, reason, .. } = event {
            sink.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((reason, audio.len()));
        }
    })
    .unwrap();
    seen
}

fn wait_for(seen: &Arc<Mutex<Vec<(EndReason, usize)>>>, count: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if seen.lock().unwrap().len() >= count {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn a_recording_of_nothing_ends_saying_nothing_was_said() {
    let ear = ear();
    let seen = endings(&ear);
    ear.set_no_speech_timeout(Duration::from_millis(300))
        .unwrap();
    ear.enable_speech().unwrap();
    ear.start().unwrap();
    ear.start_recording().unwrap();

    assert!(wait_for(&seen, 1), "no ending arrived");
    let (reason, samples) = seen.lock().unwrap()[0];
    assert_eq!(reason, EndReason::NoSpeech);
    assert!(samples > 0, "the recording carried audio anyway");
    ear.stop().unwrap();
}

#[test]
fn exactly_one_ending_arrives_per_recording() {
    let ear = ear();
    let seen = endings(&ear);
    ear.set_no_speech_timeout(Duration::from_millis(200))
        .unwrap();
    ear.enable_speech().unwrap();
    ear.start().unwrap();
    ear.start_recording().unwrap();

    assert!(wait_for(&seen, 1));
    thread::sleep(Duration::from_millis(300));
    assert_eq!(seen.lock().unwrap().len(), 1, "one recording, one ending");
    ear.stop().unwrap();
}

#[test]
fn the_application_can_end_a_recording_itself() {
    // A device producing nothing, so no audio time passes and only the
    // application can end this. The detector counts audio, not the wall
    // clock, so one running flat out would time out at once.
    let ear = EdgeEar::with_backend(Box::new(FakeBackend::starving())).expect("handle");
    let seen = endings(&ear);
    ear.enable_speech().unwrap();
    ear.start().unwrap();
    ear.start_recording().unwrap();

    thread::sleep(Duration::from_millis(100));
    ear.stop_recording().unwrap();

    assert!(wait_for(&seen, 1), "stopping produced no ending");
    assert_eq!(seen.lock().unwrap()[0].0, EndReason::Stopped);
    ear.stop().unwrap();
}

#[test]
fn recordings_can_follow_one_another() {
    let ear = ear();
    let seen = endings(&ear);
    ear.set_no_speech_timeout(Duration::from_millis(200))
        .unwrap();
    ear.enable_speech().unwrap();
    ear.start().unwrap();

    for _ in 0..2 {
        ear.start_recording().unwrap();
        thread::sleep(Duration::from_millis(400));
    }
    assert!(wait_for(&seen, 2), "expected two endings");
    ear.stop().unwrap();
}

#[test]
fn recording_before_capture_starts_says_capture_is_not_running() {
    let ear = ear();
    assert!(matches!(
        ear.start_recording().expect_err("must fail"),
        Error::NotRunning
    ));
    assert!(matches!(
        ear.stop_recording().expect_err("must fail"),
        Error::NotRunning
    ));
}

#[test]
fn recording_after_destroy_says_the_handle_is_gone() {
    let ear = ear();
    ear.start().unwrap();
    ear.destroy();
    assert!(matches!(
        ear.start_recording().expect_err("must fail"),
        Error::Destroyed
    ));
}

#[test]
fn the_detector_can_be_switched_on_and_off_at_any_time() {
    let ear = ear();
    ear.enable_speech().unwrap();
    ear.start().unwrap();
    assert!(ear.is_speech_enabled());

    ear.disable_speech().unwrap();
    assert!(!ear.is_speech_enabled());
    ear.enable_speech().unwrap();
    assert!(ear.is_speech_enabled());

    // Reading raw audio is untouched by any of it.
    ear.read(Some(Duration::from_secs(2))).expect("audio");
    ear.stop().unwrap();
}

#[test]
fn reading_audio_still_works_while_a_recording_is_open() {
    let ear = ear();
    ear.set_no_speech_timeout(Duration::from_secs(20)).unwrap();
    ear.set_max_recording(Duration::from_secs(25)).unwrap();
    ear.enable_speech().unwrap();
    ear.start().unwrap();
    ear.start_recording().unwrap();

    // What matters is that the read path keeps serving while a
    // recording is open. Whether this reader keeps up is a separate
    // question against a device running flat out.
    for _ in 0..10 {
        let chunk = ear.read(Some(Duration::from_secs(2))).expect("audio");
        assert!(!chunk.samples.is_empty(), "the read path went quiet");
    }
    ear.stop().unwrap();
}

#[test]
fn recording_settings_can_be_changed_between_recordings() {
    let ear = ear();
    ear.set_silence_duration(Duration::from_millis(500))
        .unwrap();
    ear.set_no_speech_timeout(Duration::from_millis(200))
        .unwrap();
    ear.enable_speech().unwrap();
    ear.start().unwrap();

    let seen = endings(&ear);
    ear.start_recording().unwrap();
    assert!(wait_for(&seen, 1));

    // The recording is over, so the rules can change again.
    ear.set_silence_duration(Duration::from_millis(900))
        .unwrap();
    ear.set_max_recording(Duration::from_secs(12)).unwrap();
    ear.stop().unwrap();
}

#[test]
fn changing_the_rules_mid_recording_is_refused_rather_than_ignored() {
    // Nothing arrives, so the recording stays open until told otherwise.
    let ear = EdgeEar::with_backend(Box::new(FakeBackend::starving())).expect("handle");
    ear.enable_speech().unwrap();
    ear.start().unwrap();
    ear.start_recording().unwrap();

    // Give the detector a moment to open it.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ear.is_recording() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(ear.is_recording(), "a recording should be open");

    for result in [
        ear.set_silence_duration(Duration::from_millis(100)),
        ear.set_max_recording(Duration::from_secs(5)),
        ear.set_no_speech_timeout(Duration::from_secs(5)),
        ear.set_speech_threshold(0.9),
        ear.set_pre_roll(Duration::from_millis(200)),
    ] {
        let err = result.expect_err("must be refused while a recording is open");
        assert!(matches!(err, Error::RecordingOpen { .. }), "{err}");
        assert!(
            err.to_string().contains("while a recording is open"),
            "{err}"
        );
    }

    // The old value is untouched, since nothing was applied.
    assert_eq!(
        ear.config().tunable.speech_threshold,
        0.5,
        "a refused change must leave the setting alone"
    );

    // Once the recording ends, the same calls work.
    ear.stop_recording().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while ear.is_recording() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    ear.set_silence_duration(Duration::from_millis(100))
        .unwrap();
    ear.stop().unwrap();
}
