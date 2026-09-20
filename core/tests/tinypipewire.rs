//! Integration tests for the tinypipewire audio backend.

#![cfg(all(feature = "tinypipewire-backend", target_os = "linux"))]

use std::time::Duration;

use edge_ear_core::EdgeEar;
use edge_ear_core::backend::AudioBackend;
use edge_ear_core::backend::tinypipewire_backend::TinypipewireBackend;
use edge_ear_core::config::{AudioFormat, SampleType, Target};

#[test]
fn tinypipewire_backend_creation_and_description() {
    let backend = TinypipewireBackend::new().expect("create backend");
    assert_eq!(backend.describe(), "tinypipewire");
}

#[test]
fn tinypipewire_backend_reports_supported_formats() {
    let backend = TinypipewireBackend::new().expect("create backend");
    let formats = backend.input_formats(None).expect("input formats");
    assert!(!formats.is_empty(), "expected offered formats");

    let mono_16k = AudioFormat::mono_16k();
    assert!(
        formats.iter().any(|f| f.covers(&mono_16k)),
        "16kHz mono must be covered"
    );

    let stereo_48k = AudioFormat::new(48_000, 2, SampleType::I16);
    assert!(
        formats.iter().any(|f| f.covers(&stereo_48k)),
        "48kHz stereo must be covered"
    );
}

#[test]
fn edge_ear_with_tinypipewire_constructor() {
    let ear = EdgeEar::with_tinypipewire().expect("build EdgeEar with tinypipewire");
    assert!(!ear.is_running());
}

#[test]
#[ignore = "needs a running PipeWire daemon"]
fn the_tinypipewire_backend_lists_devices_and_captures() {
    let ear = EdgeEar::with_tinypipewire().expect("handle over tinypipewire");
    let devices = ear.input_devices().expect("input devices");
    println!("PipeWire input targets: {devices:?}");

    ear.set_format(Target::Read, AudioFormat::mono_16k())
        .unwrap();
    ear.start().expect("start PipeWire capture");

    let chunk = ear
        .read(Some(Duration::from_secs(3)))
        .expect("read audio chunk");
    assert_eq!(chunk.format, AudioFormat::mono_16k());
    assert!(!chunk.samples.is_empty());

    ear.stop().expect("stop PipeWire capture");
}

#[test]
#[ignore = "needs a running PipeWire daemon"]
fn the_tinypipewire_backend_plays_audio() {
    use edge_ear_core::{Samples, SoundSource};

    let ear = EdgeEar::with_tinypipewire().expect("handle over tinypipewire");
    let pcm = SoundSource::Pcm {
        data: Samples::I16(vec![1000; 16_000]),
        sample_rate: 16_000,
        channels: 1,
    };
    ear.register_sound("beep", pcm, 0.5)
        .expect("register beep sound");

    ear.play_sound("beep", false).expect("play beep");
    assert!(ear.is_playing());

    std::thread::sleep(Duration::from_millis(200));
    ear.stop_sound().expect("stop sound");
    assert!(!ear.is_playing());
}

/// The speaker is let go while idle and taken again. Against a real
/// daemon, because it is PipeWire that has to release the sink.
#[test]
#[ignore = "needs a running PipeWire daemon"]
fn an_idle_speaker_is_released_and_taken_again() {
    use edge_ear_core::{Samples, SoundSource};

    let ear = EdgeEar::with_tinypipewire().expect("handle over tinypipewire");
    // Silence, so running this says nothing out loud.
    let quiet = SoundSource::Pcm {
        data: Samples::I16(vec![0; 16_000]),
        sample_rate: 16_000,
        channels: 1,
    };
    ear.register_sound("quiet", quiet, 1.0).expect("register");

    ear.play_sound("quiet", false).expect("first play");
    assert!(ear.is_playing());
    // Past the end of the sound and past the idle timeout, so the
    // device has been let go by the time this returns.
    std::thread::sleep(Duration::from_secs(4));
    assert!(!ear.is_playing(), "the sound has run out by now");

    // Nothing above knows whether the device is open, so what this
    // proves is only that a sound after the release still plays.
    ear.play_sound("quiet", false)
        .expect("play after the release");
    assert!(ear.is_playing(), "the speaker must be taken again");
    std::thread::sleep(Duration::from_millis(200));
    ear.stop_sound().expect("stop sound");
}
