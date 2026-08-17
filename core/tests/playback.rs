//! Playing sounds, and being told when one has finished.
//!
//! Alerts, waiting loops, and spoken replies that arrive while running
//! all go through the single owner of the speaker.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use edge_ear_core::EdgeEar;
use edge_ear_core::backend::fake::{FakeBackend, FakeFailure};
use edge_ear_core::config::SampleType;
use edge_ear_core::error::Error;
use edge_ear_core::events::Event;
use edge_ear_core::player::registry::SoundSource;

fn ear() -> EdgeEar {
    EdgeEar::with_backend(Box::new(FakeBackend::silent())).expect("handle")
}

/// A short tone, so a test can tell audio from silence.
fn tone(samples: usize) -> SoundSource {
    SoundSource::Pcm {
        data: (0..samples)
            .map(|n| ((n as f32 * 0.1).sin() * 8000.0) as i16)
            .collect(),
        sample_rate: 16_000,
        channels: 1,
        sample_type: SampleType::I16,
    }
}

fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    false
}

fn finished_ids(ear: &EdgeEar) -> Arc<Mutex<Vec<String>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    ear.on_event(move |event| {
        if let Event::SoundFinished { id } = event {
            sink.lock().unwrap_or_else(|e| e.into_inner()).push(id);
        }
    })
    .unwrap();
    seen
}

#[test]
fn playing_an_unregistered_sound_says_so() {
    let ear = ear();
    let err = ear
        .play_sound("never-registered", false)
        .expect_err("must fail");
    assert!(matches!(err, Error::UnknownSound(_)), "{err}");
}

#[test]
fn a_registered_sound_plays_and_reports_that_it_finished() {
    let ear = ear();
    let seen = finished_ids(&ear);

    ear.register_sound("alert", tone(1600), 0.8).unwrap();
    ear.play_sound("alert", false).unwrap();

    assert!(
        wait_until(|| seen.lock().unwrap().len() == 1),
        "expected one completion, saw {:?}",
        seen.lock().unwrap()
    );
    assert_eq!(seen.lock().unwrap()[0], "alert");
    assert!(!ear.is_playing(), "playback ended on its own");
}

#[test]
fn a_looping_sound_repeats_until_it_is_stopped() {
    let ear = ear();
    let seen = finished_ids(&ear);

    ear.register_sound("wait", tone(800), 0.5).unwrap();
    ear.play_sound("wait", true).unwrap();

    // Long enough that a one-shot would have ended several times over.
    thread::sleep(Duration::from_millis(200));
    assert!(ear.is_playing(), "a repeating sound keeps going");
    assert!(
        seen.lock().unwrap().is_empty(),
        "a repeating sound never finishes on its own"
    );

    ear.stop_sound().unwrap();
    assert!(wait_until(|| !ear.is_playing()));
}

#[test]
fn stopping_a_sound_does_not_report_it_as_finished() {
    let ear = ear();
    let seen = finished_ids(&ear);

    ear.register_sound("alert", tone(160_000), 1.0).unwrap();
    ear.play_sound("alert", false).unwrap();
    assert!(wait_until(|| ear.is_playing()));

    ear.stop_sound().unwrap();
    thread::sleep(Duration::from_millis(100));

    assert!(
        seen.lock().unwrap().is_empty(),
        "a sound the application stopped did not finish on its own"
    );
}

#[test]
fn reply_audio_arriving_while_running_plays_without_a_restart() {
    let ear = ear();
    ear.start().unwrap();
    let seen = finished_ids(&ear);

    // A reply at the rate speech synthesis usually produces.
    ear.register_sound(
        "reply",
        SoundSource::Pcm {
            data: (0..24_000).map(|n| ((n % 100) as i16) * 100).collect(),
            sample_rate: 24_000,
            channels: 1,
            sample_type: SampleType::I16,
        },
        1.0,
    )
    .unwrap();
    ear.play_sound("reply", false).unwrap();

    assert!(ear.is_running(), "capture kept running throughout");
    assert!(wait_until(|| seen.lock().unwrap().len() == 1));
    assert!(ear.is_running(), "and is still running afterwards");
    ear.stop().unwrap();
}

#[test]
fn a_newer_sound_takes_over_from_the_one_playing() {
    let ear = ear();
    ear.register_sound("first", tone(160_000), 1.0).unwrap();
    ear.register_sound("second", tone(160_000), 1.0).unwrap();

    ear.play_sound("first", false).unwrap();
    assert!(wait_until(|| ear.is_playing()));

    ear.play_sound("second", false).unwrap();
    // One owner of the speaker, so there is only ever one sound.
    assert!(ear.is_playing());
    ear.stop_sound().unwrap();
}

#[test]
fn a_sound_can_be_registered_before_capture_ever_starts() {
    let ear = ear();
    ear.register_sound("alert", tone(1600), 1.0).unwrap();
    ear.play_sound("alert", false).unwrap();
    assert!(wait_until(|| !ear.is_playing()));
}

#[test]
fn registering_the_same_id_again_replaces_what_was_there() {
    let ear = ear();
    ear.register_sound("reply", tone(1600), 1.0).unwrap();
    ear.register_sound("reply", tone(3200), 1.0).unwrap();

    let seen = finished_ids(&ear);
    ear.play_sound("reply", false).unwrap();
    assert!(wait_until(|| seen.lock().unwrap().len() == 1));
}

#[test]
fn a_sound_can_be_released_and_is_then_unknown() {
    let ear = ear();
    ear.register_sound("reply", tone(1600), 1.0).unwrap();
    ear.unregister_sound("reply").unwrap();

    let err = ear.play_sound("reply", false).expect_err("must fail");
    assert!(matches!(err, Error::UnknownSound(_)), "{err}");
}

#[test]
fn releasing_something_that_was_never_registered_says_so() {
    let ear = ear();
    let err = ear.unregister_sound("ghost").expect_err("must fail");
    assert!(matches!(err, Error::UnknownSound(_)), "{err}");
}

#[test]
fn a_volume_outside_the_range_is_refused() {
    let ear = ear();
    let err = ear
        .register_sound("alert", tone(1600), 5.0)
        .expect_err("must fail");
    assert!(matches!(err, Error::InvalidValue { .. }), "{err}");
}

#[test]
fn a_missing_speaker_is_reported_and_is_not_an_unknown_sound() {
    let backend = FakeBackend::failing_output(FakeFailure::NoDevice);
    let ear = EdgeEar::with_backend(Box::new(backend)).unwrap();

    let err = ear
        .register_sound("alert", tone(1600), 1.0)
        .expect_err("must fail");
    assert!(matches!(err, Error::NoDevice(_)), "{err}");
}

#[test]
fn playback_calls_are_safe_from_several_threads() {
    let ear = Arc::new(ear());
    ear.register_sound("alert", tone(16_000), 1.0).unwrap();

    let plays = Arc::new(AtomicUsize::new(0));
    let mut threads = Vec::new();
    for _ in 0..4 {
        let ear = Arc::clone(&ear);
        let plays = Arc::clone(&plays);
        threads.push(thread::spawn(move || {
            for _ in 0..20 {
                let _ = ear.play_sound("alert", false);
                let _ = ear.is_playing();
                let _ = ear.stop_sound();
                plays.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for t in threads {
        t.join().expect("playback thread");
    }
    assert_eq!(plays.load(Ordering::Relaxed), 80);
}

#[test]
fn every_playback_call_fails_once_the_handle_is_destroyed() {
    let ear = ear();
    ear.register_sound("alert", tone(1600), 1.0).unwrap();
    ear.destroy();

    assert!(matches!(
        ear.register_sound("other", tone(1600), 1.0)
            .expect_err("must fail"),
        Error::Destroyed
    ));
    assert!(matches!(
        ear.play_sound("alert", false).expect_err("must fail"),
        Error::Destroyed
    ));
    assert!(matches!(
        ear.unregister_sound("alert").expect_err("must fail"),
        Error::Destroyed
    ));
    assert!(matches!(
        ear.stop_sound().expect_err("must fail"),
        Error::Destroyed
    ));
}

/// Needs a real speaker, so it is not part of the normal run. Listen
/// for two short tones with a gap between them.
#[test]
#[ignore]
fn sounds_are_audible_on_a_real_speaker() {
    let ear = EdgeEar::new().expect("handle over the real devices");
    let seen = finished_ids(&ear);

    ear.register_sound("beep", tone(8_000), 0.6).unwrap();
    ear.play_sound("beep", false).unwrap();
    assert!(wait_until(|| seen.lock().unwrap().len() == 1));

    thread::sleep(Duration::from_millis(300));

    ear.play_sound("beep", false).unwrap();
    assert!(wait_until(|| seen.lock().unwrap().len() == 2));
    println!("played two tones through the real speaker");
}
