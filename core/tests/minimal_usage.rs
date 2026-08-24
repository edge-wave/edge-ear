//! The shortest way to get live audio, kept honest by compiling it. A
//! later change adding one more call before audio arrives breaks this,
//! and someone has to decide whether the step is worth it.

use std::time::Duration;

use edge_ear_core::EdgeEar;
use edge_ear_core::backend::fake::FakeBackend;

/// Nothing to configure, nothing to load, nothing to switch on.
#[test]
fn live_audio_takes_three_calls() {
    // 1. Make a handle. A real application writes EdgeEar::new(); a
    //    test supplies a device so it runs anywhere.
    let ear = EdgeEar::with_backend(Box::new(FakeBackend::paced())).unwrap();
    // 2. Start capture.
    ear.start().unwrap();
    // 3. Read.
    let chunk = ear.read(Some(Duration::from_secs(2))).unwrap();

    assert!(!chunk.samples.is_empty());
    ear.stop().unwrap();
}

/// The same path with the real devices, so the count holds outside a
/// test too. Needs a machine with a microphone.
#[test]
#[ignore]
fn live_audio_takes_three_calls_on_real_devices() {
    let ear = EdgeEar::new().unwrap();
    ear.start().unwrap();
    let chunk = ear.read(Some(Duration::from_secs(2))).unwrap();

    assert!(!chunk.samples.is_empty());
    ear.stop().unwrap();
}

/// Playing a sound is register then play. Nothing has to be started
/// first, because the speaker is not the microphone.
#[test]
fn playing_a_sound_takes_two_calls() {
    let ear = EdgeEar::with_backend(Box::new(FakeBackend::paced())).unwrap();

    ear.register_sound(
        "alert",
        edge_ear_core::SoundSource::Pcm {
            data: vec![2000; 1600],
            sample_rate: 16_000,
            channels: 1,
            sample_type: edge_ear_core::config::SampleType::I16,
        },
        0.8,
    )
    .unwrap();
    ear.play_sound("alert", false).unwrap();
}

/// Capturing a spoken request is switch on, start, open. The library
/// decides when the speaker stopped.
#[test]
fn capturing_a_request_takes_four_calls() {
    let ear = EdgeEar::with_backend(Box::new(FakeBackend::paced())).unwrap();

    ear.on_event(|_| {}).unwrap();
    ear.enable_speech().unwrap();
    ear.start().unwrap();
    ear.start_recording().unwrap();

    assert!(ear.is_running());
    ear.stop().unwrap();
}
