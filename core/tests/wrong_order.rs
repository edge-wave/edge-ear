//! Every public entry point, called where it cannot succeed. Wrong
//! order is normal behaviour, not misuse: each must return its own
//! named error and none may crash, hang, or wedge the handle.

use std::time::{Duration, Instant};

use edge_ear_core::Samples;
use edge_ear_core::backend::fake::FakeBackend;
use edge_ear_core::config::{AudioFormat, Target};
use edge_ear_core::error::Error;
use edge_ear_core::{EdgeEar, SoundSource};

fn ear() -> EdgeEar {
    EdgeEar::with_backend(Box::new(FakeBackend::paced())).expect("handle")
}

fn tone() -> SoundSource {
    SoundSource::Pcm {
        data: Samples::I16(vec![1000; 1600]),
        sample_rate: 16_000,
        channels: 1,
    }
}

/// Making a call on a handle and reporting only whether it was refused.
type Attempt = Box<dyn Fn(&EdgeEar) -> Result<(), Error>>;

/// One entry point, and what it should say in the state under test.
struct Call {
    name: &'static str,
    run: Attempt,
}

fn call(name: &'static str, run: impl Fn(&EdgeEar) -> Result<(), Error> + 'static) -> Call {
    Call {
        name,
        run: Box::new(run),
    }
}

/// Everything that can fail, so no state test can quietly skip one.
fn every_entry_point() -> Vec<Call> {
    vec![
        call("start", |e| e.start()),
        call("stop", |e| e.stop()),
        call("read", |e| {
            e.read(Some(Duration::from_millis(50))).map(|_| ())
        }),
        call("set_format", |e| {
            e.set_format(Target::Read, AudioFormat::mono_16k())
        }),
        call("set_input_device", |e| e.set_input_device(None)),
        call("set_output_device", |e| e.set_output_device(None)),
        call("set_ring_capacity", |e| {
            e.set_ring_capacity(Duration::from_secs(2))
        }),
        call("set_wake_threshold", |e| e.set_wake_threshold(0.5)),
        call("set_speech_threshold", |e| e.set_speech_threshold(0.5)),
        call("set_silence_duration", |e| {
            e.set_silence_duration(Duration::from_secs(1))
        }),
        call("set_max_recording", |e| {
            e.set_max_recording(Duration::from_secs(20))
        }),
        call("set_no_speech_timeout", |e| {
            e.set_no_speech_timeout(Duration::from_secs(5))
        }),
        call("set_pre_roll", |e| e.set_pre_roll(Duration::ZERO)),
        call("enable_speech", |e| e.enable_speech()),
        call("disable_speech", |e| e.disable_speech()),
        call("start_recording", |e| e.start_recording()),
        call("stop_recording", |e| e.stop_recording()),
        call("register_sound", |e| e.register_sound("x", tone(), 1.0)),
        call("unregister_sound", |e| e.unregister_sound("x")),
        call("play_sound", |e| e.play_sound("x", false)),
        call("stop_sound", |e| e.stop_sound()),
        call("input_devices", |e| e.input_devices().map(|_| ())),
        call("output_devices", |e| e.output_devices().map(|_| ())),
        call("on_event", |e| e.on_event(|_| {})),
    ]
}

#[test]
fn every_entry_point_refuses_once_the_handle_is_destroyed() {
    for entry in every_entry_point() {
        let ear = ear();
        ear.destroy();
        let err = (entry.run)(&ear)
            .expect_err(&format!("{} should refuse a destroyed handle", entry.name));
        assert!(
            matches!(err, Error::Destroyed),
            "{} gave {err} instead of saying the handle is gone",
            entry.name
        );
    }
}

#[test]
fn the_calls_that_need_capture_say_so_when_it_is_not_running() {
    let needs_capture = ["read", "stop", "start_recording", "stop_recording"];
    for entry in every_entry_point() {
        if !needs_capture.contains(&entry.name) {
            continue;
        }
        let ear = ear();
        let err =
            (entry.run)(&ear).expect_err(&format!("{} should need capture running", entry.name));
        assert!(
            matches!(err, Error::NotRunning),
            "{} gave {err} instead of saying capture is not running",
            entry.name
        );
    }
}

#[test]
fn the_settings_fixed_at_start_refuse_once_capture_is_running() {
    // The speaker is not opened by starting capture, so what it is
    // opened at is fixed by its own rule rather than this one.
    let fixed_at_start = ["set_format", "set_input_device", "set_ring_capacity"];
    for entry in every_entry_point() {
        if !fixed_at_start.contains(&entry.name) {
            continue;
        }
        let ear = ear();
        ear.start().unwrap();
        let err =
            (entry.run)(&ear).expect_err(&format!("{} should be fixed once running", entry.name));
        assert!(
            matches!(err, Error::RunningNotAllowed { .. }),
            "{} gave {err} instead of saying it is fixed at start",
            entry.name
        );
        assert!(
            err.to_string().contains("stop capture first"),
            "{} did not say what to do: {err}",
            entry.name
        );
    }
}

#[test]
fn the_settings_a_recording_follows_refuse_while_one_is_open() {
    let followed_by_a_recording = [
        "set_speech_threshold",
        "set_silence_duration",
        "set_max_recording",
        "set_no_speech_timeout",
        "set_pre_roll",
    ];
    for entry in every_entry_point() {
        if !followed_by_a_recording.contains(&entry.name) {
            continue;
        }
        // Nothing arrives, so the recording stays open.
        let ear = EdgeEar::with_backend(Box::new(FakeBackend::starving())).unwrap();
        ear.enable_speech().unwrap();
        ear.start().unwrap();
        ear.start_recording().unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while !ear.is_recording() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ear.is_recording(), "a recording should be open");

        let err = (entry.run)(&ear).expect_err(&format!(
            "{} should be refused while a recording is open",
            entry.name
        ));
        assert!(
            matches!(err, Error::RecordingOpen { .. }),
            "{} gave {err} instead of saying a recording is open",
            entry.name
        );
    }
}

#[test]
fn starting_twice_says_it_is_already_running() {
    let ear = ear();
    ear.start().unwrap();
    let err = ear.start().expect_err("must refuse");
    assert!(matches!(err, Error::AlreadyRunning), "{err}");
}

#[test]
fn playing_or_releasing_a_sound_nobody_registered_says_so() {
    let ear = ear();
    for result in [
        ear.play_sound("ghost", false),
        ear.unregister_sound("ghost"),
    ] {
        let err = result.expect_err("must refuse");
        assert!(matches!(err, Error::UnknownSound(_)), "{err}");
    }
}

#[test]
fn a_wrong_order_call_leaves_the_handle_usable() {
    let ear = ear();

    // A pile of calls made where they cannot succeed.
    let _ = ear.read(Some(Duration::from_millis(10)));
    let _ = ear.stop();
    let _ = ear.start_recording();
    let _ = ear.play_sound("ghost", false);
    let _ = ear.set_wake_threshold(9.0);

    // The handle still works afterwards.
    ear.start().unwrap();
    assert!(ear.is_running());
    ear.read(Some(Duration::from_secs(2))).expect("audio");
    ear.stop().unwrap();
}

#[test]
fn a_refused_setting_leaves_the_old_value_alone() {
    let ear = ear();
    ear.set_wake_threshold(0.7).unwrap();
    ear.set_speech_threshold(0.3).unwrap();

    assert!(ear.set_wake_threshold(5.0).is_err());
    assert!(ear.set_speech_threshold(-1.0).is_err());

    let config = ear.config();
    assert_eq!(config.tunable.wake_threshold, 0.7);
    assert_eq!(config.tunable.speech_threshold, 0.3);
}

#[test]
fn nothing_hangs_when_called_in_the_wrong_order() {
    let started = Instant::now();
    for entry in every_entry_point() {
        let ear = ear();
        let _ = (entry.run)(&ear);
        ear.destroy();
        let _ = (entry.run)(&ear);
    }
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the sweep took {:?}, which suggests something waited",
        started.elapsed()
    );
}

/// The speaker opens on the first sound and stays open, so what it is
/// opened at is fixed from then rather than from the start of capture.
#[test]
fn the_settings_fixed_at_the_speaker_refuse_once_it_is_open() {
    let ear = ear();
    ear.start().unwrap();
    // Capture running is not the speaker being open.
    ear.set_output_device(None)
        .expect("the speaker has not been opened yet");

    ear.register_sound("x", tone(), 1.0).unwrap();

    for (what, result) in [
        ("set_output_device", ear.set_output_device(None)),
        (
            "set_output_device_format",
            ear.set_output_device_format(Some(AudioFormat::mono_16k())),
        ),
    ] {
        let err = result.expect_err(&format!("{what} should be fixed once the speaker is open"));
        assert!(
            matches!(err, Error::RunningNotAllowed { .. }),
            "{what} gave {err} instead of saying it is fixed"
        );
    }
}
