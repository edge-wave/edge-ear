//! Echo cancellation through the whole handle: what the speaker plays reaches the canceller, and
//! what the microphone hears reaches every consumer only after the canceller has cleaned it.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use edge_ear_core::backend::fake::{FakeBackend, FakeSetup};
use edge_ear_core::config::AudioFormat;
use edge_ear_core::echo::EchoCanceller;
use edge_ear_core::error::Error;
use edge_ear_core::{EdgeEar, Samples, SoundSource};

#[derive(Default)]
struct Log {
    rendered: Vec<f32>,
    captured: usize,
    resets: usize,
}

/// Keeps what the speaker played and silences the microphone, so its work shows downstream.
struct Silencer(Arc<Mutex<Log>>);

impl EchoCanceller for Silencer {
    fn sample_rate(&self) -> u32 {
        16_000
    }
    fn render(&mut self, frame: &[f32]) {
        self.0.lock().unwrap().rendered.extend_from_slice(frame);
    }
    fn capture(&mut self, frame: &mut [f32]) {
        self.0.lock().unwrap().captured += 1;
        frame.fill(0.0);
    }
    fn reset(&mut self) {
        self.0.lock().unwrap().resets += 1;
    }
}

/// A microphone that hears a steady level for ten seconds, at the pace a real one would.
fn ear_hearing_something() -> EdgeEar {
    let backend = FakeBackend::new(FakeSetup {
        input_audio: vec![8_000; 160_000],
        input_format: Some(AudioFormat::mono_16k()),
        block_samples: 160,
        paced: true,
        ..Default::default()
    });
    EdgeEar::with_backend(Box::new(backend)).expect("handle")
}

fn silencer(ear: &EdgeEar) -> Arc<Mutex<Log>> {
    let log = Arc::new(Mutex::new(Log::default()));
    ear.set_echo_canceller(Box::new(Silencer(Arc::clone(&log))))
        .unwrap();
    log
}

fn tone(samples: usize) -> SoundSource {
    SoundSource::Pcm {
        data: Samples::I16(vec![4_000; samples]),
        sample_rate: 16_000,
        channels: 1,
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

#[test]
fn what_the_speaker_plays_reaches_the_canceller() {
    let ear = ear_hearing_something();
    let log = silencer(&ear);
    ear.start().unwrap();
    ear.register_sound("beep", tone(1_600), 1.0).unwrap();
    ear.play_sound("beep", false).unwrap();

    let heard = |log: &Log| {
        log.rendered
            .iter()
            .any(|s| (s - 4_000.0 / 32_768.0).abs() < 1e-3)
    };
    assert!(
        wait_until(|| heard(&log.lock().unwrap())),
        "the beep never reached the canceller"
    );
}

#[test]
fn every_reader_gets_the_cleaned_microphone() {
    let ear = ear_hearing_something();
    let log = silencer(&ear);
    ear.start().unwrap();

    for _ in 0..20 {
        let chunk = ear.read(Some(Duration::from_secs(2))).unwrap();
        assert!(
            chunk.samples.as_i16().unwrap().iter().all(|s| *s == 0),
            "the microphone reached the reader as it was heard"
        );
    }
    assert!(log.lock().unwrap().captured >= 20);
}

#[test]
fn without_a_canceller_the_microphone_is_left_alone() {
    let ear = ear_hearing_something();
    assert!(!ear.is_echo_cancellation_enabled());
    ear.start().unwrap();

    let chunk = ear.read(Some(Duration::from_secs(2))).unwrap();
    assert!(chunk.samples.as_i16().unwrap().iter().all(|s| *s == 8_000));
}

#[test]
fn the_canceller_starts_afresh_with_each_capture() {
    let ear = ear_hearing_something();
    let log = silencer(&ear);
    assert!(ear.is_echo_cancellation_enabled());

    ear.start().unwrap();
    ear.stop().unwrap();
    ear.start().unwrap();
    assert_eq!(log.lock().unwrap().resets, 2);
}

#[test]
fn the_choice_is_made_before_capture_starts() {
    let ear = ear_hearing_something();
    ear.start().unwrap();
    let err = ear.set_echo_cancellation(false).expect_err("must fail");
    assert!(matches!(err, Error::RunningNotAllowed { .. }), "{err}");

    ear.stop().unwrap();
    silencer(&ear);
    ear.set_echo_cancellation(false).unwrap();
    assert!(!ear.is_echo_cancellation_enabled());
}

#[cfg(feature = "webrtc-aec")]
#[test]
fn webrtc_runs_with_capture_when_asked_for() {
    let ear = ear_hearing_something();
    ear.set_echo_cancellation(true).unwrap();
    assert!(ear.is_echo_cancellation_enabled());
    ear.start().unwrap();

    let chunk = ear.read(Some(Duration::from_secs(2))).unwrap();
    assert_eq!(chunk.format, AudioFormat::mono_16k());
}

/// How much of a noise played into a room comes back through `read`, from its third second on.
#[cfg(feature = "webrtc-aec")]
fn echo_read_back(cancel: bool, microphone: AudioFormat, speaker: AudioFormat) -> f64 {
    let backend = FakeBackend::new(FakeSetup {
        input_format: Some(microphone),
        block_samples: microphone.sample_rate as usize / 100 * usize::from(microphone.channels),
        output_defaults: vec![speaker],
        paced: true,
        echo_gain: Some(0.5),
        // A device's own latency, so the room hears each block after it was handed over.
        output_delay: Duration::from_millis(20),
        ..Default::default()
    });
    let ear = EdgeEar::with_backend(Box::new(backend)).expect("handle");
    ear.set_echo_cancellation(cancel).unwrap();
    ear.start().unwrap();

    let mut seed = 7u32;
    let noise = (0..64_000)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((seed >> 16) as i16).wrapping_sub(i16::MAX / 2) / 4
        })
        .collect();
    let sound = SoundSource::Pcm {
        data: Samples::I16(noise),
        sample_rate: 16_000,
        channels: 1,
    };
    ear.register_sound("noise", sound, 1.0).unwrap();
    ear.play_sound("noise", false).unwrap();

    let (mut read, mut energy) = (0, 0.0);
    while read < 56_000 {
        let chunk = ear.read(Some(Duration::from_secs(2))).unwrap();
        for sample in chunk.samples.as_i16().unwrap() {
            if read >= 32_000 {
                energy += f64::from(*sample).powi(2);
            }
            read += 1;
        }
    }
    energy
}

#[cfg(feature = "webrtc-aec")]
fn assert_echo_taken_out(microphone: AudioFormat, speaker: AudioFormat) {
    let left_in = echo_read_back(false, microphone, speaker);
    let taken_out = echo_read_back(true, microphone, speaker);
    let reduction_db = 10.0 * (left_in / taken_out.max(1.0)).log10();
    assert!(
        reduction_db > 10.0,
        "only {reduction_db:.1} dB of the room echo was taken out"
    );
}

#[cfg(feature = "webrtc-aec")]
#[test]
#[ignore = "runs in real time, which shared CI runners cannot keep"]
fn webrtc_takes_the_room_echo_out_of_what_is_read() {
    assert_echo_taken_out(AudioFormat::mono_16k(), AudioFormat::mono_16k());
}

#[cfg(feature = "webrtc-aec")]
#[test]
#[ignore = "runs in real time, which shared CI runners cannot keep"]
fn webrtc_does_so_across_resampling_on_both_sides() {
    use edge_ear_core::config::SampleType;
    let microphone = AudioFormat::new(48_000, 1, SampleType::I16);
    let speaker = AudioFormat::new(48_000, 2, SampleType::I16);
    assert_echo_taken_out(microphone, speaker);
}

#[cfg(not(feature = "webrtc-aec"))]
#[test]
fn a_build_without_webrtc_refuses_it() {
    let ear = ear_hearing_something();
    let err = ear.set_echo_cancellation(true).expect_err("must fail");
    assert!(matches!(err, Error::InvalidValue { .. }), "{err}");
    assert!(!ear.is_echo_cancellation_enabled());
}
