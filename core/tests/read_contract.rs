//! The read surface, checked against its contract.
//!
//! Nothing here switches on a detector. An application that only reads
//! raw audio must never need to know they exist.

use std::time::Duration;

use edge_ear_core::EdgeEar;
use edge_ear_core::backend::fake::FakeBackend;
use edge_ear_core::config::{AudioFormat, SampleType, Target};
use edge_ear_core::error::Error;

fn ear() -> EdgeEar {
    EdgeEar::with_backend(Box::new(FakeBackend::silent())).expect("handle")
}

#[test]
fn read_returns_the_next_block_with_no_detector_enabled() {
    let ear = ear();
    ear.start().unwrap();

    let chunk = ear.read(Some(Duration::from_secs(2))).expect("audio");
    assert!(!chunk.samples.is_empty());
    assert_eq!(chunk.format, AudioFormat::mono_16k());
    assert_eq!(chunk.dropped_before, 0);

    ear.stop().unwrap();
}

#[test]
fn read_keeps_working_across_a_stop_and_start() {
    let ear = ear();
    for _ in 0..3 {
        ear.start().unwrap();
        ear.read(Some(Duration::from_secs(2))).expect("audio");
        ear.stop().unwrap();
    }
}

#[test]
fn read_before_start_says_capture_is_not_running() {
    let ear = ear();
    let err = ear.read(None).expect_err("must fail");
    assert!(matches!(err, Error::NotRunning), "{err}");
}

#[test]
fn read_after_stop_says_capture_is_not_running() {
    let ear = ear();
    ear.start().unwrap();
    ear.stop().unwrap();
    let err = ear.read(None).expect_err("must fail");
    assert!(matches!(err, Error::NotRunning), "{err}");
}

#[test]
fn read_after_destroy_says_the_handle_is_gone() {
    let ear = ear();
    ear.start().unwrap();
    ear.destroy();
    let err = ear.read(None).expect_err("must fail");
    assert!(matches!(err, Error::Destroyed), "{err}");
}

#[test]
fn the_delivered_format_is_the_one_that_was_asked_for() {
    let ear = ear();
    let wanted = AudioFormat::new(8_000, 2, SampleType::F32);
    ear.set_format(Target::Read, wanted).unwrap();
    ear.start().unwrap();

    let chunk = ear.read(Some(Duration::from_secs(2))).expect("audio");
    assert_eq!(chunk.format, wanted);
    assert!(chunk.samples.as_f32().is_some(), "expected float samples");
    assert_eq!(chunk.samples.len() % 2, 0, "stereo comes in pairs");
}
