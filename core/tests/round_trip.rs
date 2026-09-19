//! The whole hands-free journey: the wake word, the alert, the talking,
//! the stopping, the audio. The only place the four parts are asked to
//! work as one thing rather than each on its own.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use edge_ear_core::Samples;
use edge_ear_core::backend::fake::{FakeBackend, FakeSetup};
use edge_ear_core::config::AudioFormat;
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
        data: Samples::I16(vec![3000; 3200]), // 200 ms
        sample_rate: 16_000,
        channels: 1,
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
    ear.add_wake_model("hey_jarvis", &dir.join("hey_jarvis_v0.1.onnx"))
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
    ear.add_wake_model("hey_jarvis", &dir.join("hey_jarvis_v0.1.onnx"))
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
    ear.add_wake_model("hey_jarvis", &dir.join("hey_jarvis_v0.1.onnx"))
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
            data: Samples::I16(vec![3000; 16_000]), // one second
            sample_rate: 16_000,
            channels: 1,
        },
        0.4,
    )
    .unwrap();
    ear.load_wake_features(
        &dir.join("melspectrogram.onnx"),
        &dir.join("embedding_model.onnx"),
    )
    .unwrap();
    ear.add_wake_model("hey_jarvis", &dir.join("hey_jarvis_v0.1.onnx"))
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
    ear.add_wake_model("hey_jarvis", &dir.join("hey_jarvis_v0.1.onnx"))
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

/// A wake word said out loud, so the wake path really runs. Set it to
/// a wav of the phrase the model was trained on.
fn spoken_wake_word() -> Option<Vec<i16>> {
    let path = std::env::var("EDGE_EAR_WAKE_WAV").ok()?;
    let bytes = std::fs::read(path).expect("the wake word wav");
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        if &bytes[at..at + 4] == b"data" {
            let end = (at + 8 + size).min(bytes.len());
            return Some(
                bytes[at + 8..end]
                    .chunks_exact(2)
                    .map(|c| i16::from_le_bytes([c[0], c[1]]))
                    .collect(),
            );
        }
        at += 8 + size + (size & 1);
    }
    panic!("no data chunk in the wake word wav")
}

/// Silence in front so there is history to reach back into, and behind
/// so the recording has room to end on its own.
fn heard(word: Vec<i16>) -> EdgeEar {
    let mut audio = vec![0i16; 16_000];
    audio.extend(word);
    audio.extend(vec![0i16; 64_000]);
    EdgeEar::with_backend(Box::new(FakeBackend::new(FakeSetup {
        input_audio: audio,
        input_format: Some(AudioFormat::mono_16k()),
        block_samples: 160,
        paced: true,
        ..Default::default()
    })))
    .expect("handle")
}

/// Samples handed over beyond the ones the recording counted. That
/// difference came from before the recording opened.
fn wake_pre_roll(waits_for_alert: bool) -> Option<usize> {
    let dir = model_dir()?;
    let word = spoken_wake_word()?;
    let ear = heard(word);
    ear.register_sound("beep", alert(), 0.6).unwrap();
    ear.load_wake_features(
        &dir.join("melspectrogram.onnx"),
        &dir.join("embedding_model.onnx"),
    )
    .unwrap();
    ear.add_wake_model("hey_jarvis", &dir.join("hey_jarvis_v0.1.onnx"))
        .unwrap();
    ear.set_no_speech_timeout(Duration::from_millis(600))
        .unwrap();
    ear.set_pre_roll(Duration::from_millis(500)).unwrap();
    ear.set_wake_recording_waits_for_alert(waits_for_alert)
        .unwrap();

    let told = Arc::new(Mutex::new(None::<(usize, Duration)>));
    let sink = Arc::clone(&told);
    ear.on_event(move |event| {
        if let Event::SpeechEnded {
            audio, duration, ..
        } = event
        {
            let mut slot = sink.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                *slot = Some((audio.len(), duration));
            }
        }
    })
    .unwrap();
    ear.enable_wake(Some("beep")).unwrap();
    ear.enable_speech().unwrap();
    ear.start().unwrap();

    assert!(
        wait_until(15, || told
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()),
        "the wake word was never heard"
    );
    let (samples, counted) = told
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expect("checked above");
    ear.destroy();
    Some(samples.saturating_sub((counted.as_secs_f64() * 16_000.0) as usize))
}

#[test]
#[ignore]
fn a_wake_word_recording_reaches_back_like_any_other() {
    let Some(extra) = wake_pre_roll(false) else {
        println!("set EDGE_EAR_WAKE_DIR and EDGE_EAR_WAKE_WAV to run this");
        return;
    };
    let asked = (0.5 * 16_000.0) as usize;
    assert!(
        (asked..asked + 2048).contains(&extra),
        "asked for {asked} samples of history and got {extra}"
    );
}

#[test]
#[ignore]
fn waiting_for_the_alert_puts_the_pre_roll_out_of_reach() {
    let Some(extra) = wake_pre_roll(true) else {
        println!("set EDGE_EAR_WAKE_DIR and EDGE_EAR_WAKE_WAV to run this");
        return;
    };
    assert_eq!(
        extra, 0,
        "the recording began again at the alert, so nothing should sit in front of it"
    );
}

/// An alert cut short still ends the wait behind it. Only the wake word
/// opens a recording that waits, so the word has to be heard.
#[test]
#[ignore]
fn cutting_the_alert_short_still_lets_the_recording_end() {
    let (Some(dir), Some(word)) = (model_dir(), spoken_wake_word()) else {
        println!("set EDGE_EAR_WAKE_DIR and EDGE_EAR_WAKE_WAV to run this");
        return;
    };
    let ear = heard(word);
    // Long enough that it is certainly still playing when it is cut.
    ear.register_sound(
        "beep",
        SoundSource::Pcm {
            data: Samples::I16(vec![3000; 160_000]),
            sample_rate: 16_000,
            channels: 1,
        },
        0.4,
    )
    .unwrap();
    ear.load_wake_features(
        &dir.join("melspectrogram.onnx"),
        &dir.join("embedding_model.onnx"),
    )
    .unwrap();
    ear.add_wake_model("hey_jarvis", &dir.join("hey_jarvis_v0.1.onnx"))
        .unwrap();

    let seen = watch(&ear);
    ear.set_no_speech_timeout(Duration::from_millis(300))
        .unwrap();
    ear.set_max_recording(Duration::from_secs(20)).unwrap();
    ear.enable_wake(Some("beep")).unwrap();
    ear.enable_speech().unwrap();
    ear.start().unwrap();

    assert!(
        wait_until(15, || !seen.lock().unwrap().order.is_empty()),
        "the wake word was never heard"
    );
    thread::sleep(Duration::from_millis(200));
    ear.stop_sound().unwrap();

    assert!(
        wait_until(10, || !seen.lock().unwrap().endings.is_empty()),
        "the recording never ended after the alert was cut"
    );
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.endings[0].0,
        EndReason::NoSpeech,
        "it ran to the length cap instead of noticing the quiet"
    );
}

/// A setting changed while capture runs must reach whichever path
/// opens the next recording. Both are asked the same question.
fn timeout_a_recording_followed(by_wake: bool) -> Option<Duration> {
    let dir = model_dir()?;
    let word = spoken_wake_word()?;
    let ear = heard(word);
    ear.load_wake_features(
        &dir.join("melspectrogram.onnx"),
        &dir.join("embedding_model.onnx"),
    )
    .unwrap();
    ear.add_wake_model("hey_jarvis", &dir.join("hey_jarvis_v0.1.onnx"))
        .unwrap();
    // What start would have carried off, had it carried anything.
    ear.set_no_speech_timeout(Duration::from_secs(5)).unwrap();

    let told = Arc::new(Mutex::new(None::<Duration>));
    let sink = Arc::clone(&told);
    ear.on_event(move |event| {
        if let Event::SpeechEnded { duration, .. } = event {
            let mut slot = sink.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                *slot = Some(duration);
            }
        }
    })
    .unwrap();
    if by_wake {
        ear.enable_wake(None).unwrap();
    }
    ear.enable_speech().unwrap();
    ear.start().unwrap();

    ear.set_no_speech_timeout(Duration::from_millis(400))
        .unwrap();
    if !by_wake {
        ear.start_recording().unwrap();
    }

    assert!(
        wait_until(20, || told
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()),
        "no recording ever came back"
    );
    let counted = told
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expect("checked above");
    ear.destroy();
    Some(counted)
}

#[test]
#[ignore]
fn a_wake_word_recording_follows_a_timeout_changed_while_running() {
    let Some(counted) = timeout_a_recording_followed(true) else {
        println!("set EDGE_EAR_WAKE_DIR and EDGE_EAR_WAKE_WAV to run this");
        return;
    };
    assert!(
        counted < Duration::from_secs(1),
        "counted {counted:?}, so it followed the timeout in place at start"
    );
}

#[test]
#[ignore]
fn a_recording_opened_by_hand_follows_it_too() {
    let Some(counted) = timeout_a_recording_followed(false) else {
        println!("set EDGE_EAR_WAKE_DIR and EDGE_EAR_WAKE_WAV to run this");
        return;
    };
    assert!(
        counted < Duration::from_secs(1),
        "counted {counted:?}, so it followed the timeout in place at start"
    );
}

/// Two words listened for together: the one held back by its own
/// threshold stays quiet, and the other is named when heard.
#[test]
#[ignore]
fn among_several_words_the_one_heard_is_named() {
    let (Some(dir), Some(word)) = (model_dir(), spoken_wake_word()) else {
        println!("set EDGE_EAR_WAKE_DIR and EDGE_EAR_WAKE_WAV to run this");
        return;
    };
    let ear = heard(word);
    ear.load_wake_features(
        &dir.join("melspectrogram.onnx"),
        &dir.join("embedding_model.onnx"),
    )
    .unwrap();
    for name in ["first", "second"] {
        ear.add_wake_model(name, &dir.join("hey_jarvis_v0.1.onnx"))
            .unwrap();
    }
    assert_eq!(ear.wake_models(), ["first", "second"]);
    ear.set_wake_word_threshold("first", Some(1.0)).unwrap();

    let names = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&names);
    ear.on_event(move |event| {
        if let Event::WakeDetected { word, .. } = event {
            sink.lock().unwrap_or_else(|e| e.into_inner()).push(word);
        }
    })
    .unwrap();
    ear.enable_wake(None).unwrap();
    ear.start().unwrap();

    assert!(
        wait_until(15, || !names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()),
        "the wake word was never heard"
    );
    assert_eq!(names.lock().unwrap()[0], "second");
    assert!(
        ear.wake_score("first").unwrap().is_some(),
        "first was never scored"
    );
    ear.destroy();
}
