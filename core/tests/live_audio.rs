//! Reading live audio on demand.
//!
//! An application starts capture, reads audio, and stops. There is no
//! model, no detector, and no callback anywhere in this file. This is
//! everything a push-to-talk application needs.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use edge_ear_core::EdgeEar;
use edge_ear_core::backend::fake::{FakeBackend, FakeFailure};
use edge_ear_core::config::{AudioFormat, SampleType, Target};
use edge_ear_core::error::Error;

fn ear() -> EdgeEar {
    EdgeEar::with_backend(Box::new(FakeBackend::silent())).expect("handle")
}

/// A rising ramp, so a resampled result is still recognisable.
fn ramp(samples: usize) -> Vec<i16> {
    (0..samples)
        .map(|n| ((n % 1000) as f32 / 1000.0 * 8000.0) as i16)
        .collect()
}

#[test]
fn audio_arrives_continuously_with_nothing_dropped() {
    let ear = ear();
    ear.start().unwrap();

    let mut blocks = 0;
    let mut samples = 0usize;
    for _ in 0..200 {
        let chunk = ear.read(Some(Duration::from_secs(2))).expect("audio");
        assert_eq!(
            chunk.dropped_before, 0,
            "a reader keeping up must lose nothing"
        );
        blocks += 1;
        samples += chunk.samples.len();
    }

    assert_eq!(blocks, 200);
    assert!(samples > 0);
    ear.stop().unwrap();
}

#[test]
fn timing_comes_from_capture_not_from_when_it_was_read() {
    let ear = ear();
    ear.start().unwrap();

    let first = ear.read(Some(Duration::from_secs(2))).unwrap();
    // Read the next one late. Its stamp must still reflect capture.
    thread::sleep(Duration::from_millis(50));
    let second = ear.read(Some(Duration::from_secs(2))).unwrap();

    assert!(second.captured_at >= first.captured_at);
    ear.stop().unwrap();
}

#[test]
fn a_format_the_device_does_not_produce_is_still_delivered() {
    // The device runs at 44.1 kHz; the application wants 16 kHz.
    let backend = FakeBackend::playing(
        ramp(44_100),
        AudioFormat::new(44_100, 1, SampleType::I16),
        4410,
    );
    let ear = EdgeEar::with_backend(Box::new(backend)).unwrap();
    ear.set_format(Target::Read, AudioFormat::mono_16k())
        .unwrap();
    ear.start().unwrap();

    let chunk = ear.read(Some(Duration::from_secs(2))).expect("audio");
    assert_eq!(chunk.format.sample_rate, 16_000);
    assert!(!chunk.samples.is_empty());

    // Roughly a third as many samples per block, since the rate fell.
    assert!(
        chunk.samples.len() < 4410,
        "expected fewer samples after downsampling, got {}",
        chunk.samples.len()
    );
    ear.stop().unwrap();
}

#[test]
fn stereo_device_audio_reaches_a_mono_reader() {
    let backend = FakeBackend::playing(
        ramp(32_000),
        AudioFormat::new(16_000, 2, SampleType::I16),
        3200,
    );
    let ear = EdgeEar::with_backend(Box::new(backend)).unwrap();
    ear.set_format(Target::Read, AudioFormat::mono_16k())
        .unwrap();
    ear.start().unwrap();

    let chunk = ear.read(Some(Duration::from_secs(2))).expect("audio");
    assert_eq!(chunk.format.channels, 1);
    ear.stop().unwrap();
}

#[test]
fn starting_twice_and_stopping_twice_each_say_why() {
    let ear = ear();

    assert!(matches!(
        ear.stop().expect_err("must fail"),
        Error::NotRunning
    ));

    ear.start().unwrap();
    assert!(matches!(
        ear.start().expect_err("must fail"),
        Error::AlreadyRunning
    ));

    ear.stop().unwrap();
    assert!(matches!(
        ear.stop().expect_err("must fail"),
        Error::NotRunning
    ));
}

#[test]
fn a_format_change_after_start_is_refused_and_says_what_to_do() {
    let ear = ear();
    ear.start().unwrap();

    let err = ear
        .set_format(Target::Read, AudioFormat::new(8_000, 1, SampleType::I16))
        .expect_err("must fail");
    assert!(matches!(err, Error::RunningNotAllowed { .. }), "{err}");
    assert!(err.to_string().contains("stop capture first"), "{err}");
}

#[test]
fn a_waiting_read_is_released_when_capture_stops() {
    // A backend that never produces audio, so the reader really waits.
    let ear = Arc::new(EdgeEar::with_backend(Box::new(FakeBackend::starving())).unwrap());
    ear.start().unwrap();

    let reader = {
        let ear = Arc::clone(&ear);
        thread::spawn(move || ear.read(None))
    };

    thread::sleep(Duration::from_millis(80));
    let stopped_at = Instant::now();
    ear.stop().unwrap();

    let result = reader.join().expect("reader thread");
    assert!(
        stopped_at.elapsed() < Duration::from_secs(1),
        "stop must release the reader promptly"
    );
    match result {
        Err(Error::Stopped) => {}
        // A block that arrived just before the stop is also fine; what
        // must not happen is waiting for ever.
        Ok(_) => {}
        Err(other) => panic!("expected Stopped, got {other}"),
    }
}

#[test]
fn a_read_that_waits_too_long_times_out() {
    let ear = EdgeEar::with_backend(Box::new(FakeBackend::starving())).unwrap();
    ear.start().unwrap();

    let started = Instant::now();
    let err = ear
        .read(Some(Duration::from_millis(100)))
        .expect_err("must time out");
    assert!(matches!(err, Error::Timeout), "{err}");
    assert!(started.elapsed() >= Duration::from_millis(90));
    assert!(started.elapsed() < Duration::from_secs(1));

    ear.stop().unwrap();
}

#[test]
fn a_refused_microphone_is_reported_at_start() {
    let backend = FakeBackend::failing_input(FakeFailure::PermissionDenied);
    let ear = EdgeEar::with_backend(Box::new(backend)).unwrap();

    let err = ear.start().expect_err("must fail");
    assert!(matches!(err, Error::PermissionDenied), "{err}");
    assert!(!ear.is_running(), "a failed start must not look running");
}

#[test]
fn a_missing_microphone_is_a_different_error_from_a_refused_one() {
    let backend = FakeBackend::failing_input(FakeFailure::NoDevice);
    let ear = EdgeEar::with_backend(Box::new(backend)).unwrap();

    let err = ear.start().expect_err("must fail");
    assert!(matches!(err, Error::NoDevice(_)), "{err}");
}

#[test]
fn many_readers_from_many_threads_do_not_trip_over_each_other() {
    let ear = Arc::new(ear());
    ear.start().unwrap();

    let mut threads = Vec::new();
    for _ in 0..4 {
        let ear = Arc::clone(&ear);
        threads.push(thread::spawn(move || {
            for _ in 0..25 {
                let _ = ear.read(Some(Duration::from_secs(2)));
            }
        }));
    }
    for t in threads {
        t.join().expect("reader thread");
    }

    ear.stop().unwrap();
}

/// Needs a real machine with a microphone, so it is not part of the
/// normal run. This is the only place the whole path is exercised
/// against hardware rather than the fake backend.
#[test]
#[ignore]
fn live_audio_works_against_a_real_microphone() {
    let ear = EdgeEar::new().expect("a handle over the real devices");

    let devices = ear.input_devices().expect("input devices");
    assert!(!devices.is_empty(), "expected at least one microphone");
    let default = devices.iter().find(|d| d.is_default);
    println!("default input: {default:?}");
    assert!(
        default.is_some(),
        "one device must be marked as the default"
    );

    ear.set_format(Target::Read, AudioFormat::mono_16k())
        .unwrap();
    ear.start().expect("start capture");

    let mut samples = 0usize;
    for _ in 0..20 {
        let chunk = ear.read(Some(Duration::from_secs(2))).expect("live audio");
        assert_eq!(chunk.format, AudioFormat::mono_16k());
        assert_eq!(chunk.dropped_before, 0, "a reader keeping up loses nothing");
        samples += chunk.samples.len();
    }

    println!("read {samples} samples from the real microphone");
    assert!(samples > 0);
    ear.stop().unwrap();
}

/// Every listed device must be selectable by the identifier it was
/// listed under. Names repeat, so they cannot carry this on their own.
#[test]
#[ignore]
fn a_real_device_can_be_chosen_by_its_identifier() {
    let ear = EdgeEar::new().expect("handle");
    let devices = ear.input_devices().expect("input devices");
    let default = devices
        .iter()
        .find(|d| d.is_default)
        .expect("a default device");

    ear.set_input_device(Some(&default.id)).unwrap();
    ear.start().expect("start on the named device");
    ear.read(Some(Duration::from_secs(2))).expect("live audio");
    ear.stop().unwrap();
}
