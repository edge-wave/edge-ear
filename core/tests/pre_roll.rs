//! Audio from before the recording was opened. A recording says how
//! much it counted after opening, so anything past that came from the
//! history.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use edge_ear_core::EdgeEar;
use edge_ear_core::backend::fake::FakeBackend;
use edge_ear_core::events::Event;

const RATE: f64 = 16_000.0;

fn ear() -> EdgeEar {
    EdgeEar::with_backend(Box::new(FakeBackend::paced())).expect("handle")
}

/// Samples handed over beyond the ones the recording itself counted.
/// That difference is the pre-roll and nothing else.
fn extra_samples(pre_roll: Duration, settle: Duration) -> usize {
    let ear = ear();
    ear.set_no_speech_timeout(Duration::from_millis(500))
        .unwrap();
    ear.set_pre_roll(pre_roll).unwrap();

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

    ear.start().unwrap();
    ear.enable_speech().unwrap();

    // Let history build up before asking to record. Without this there
    // is nothing behind the open moment to reach back into.
    let until = Instant::now() + settle;
    while Instant::now() < until {
        thread::sleep(Duration::from_millis(10));
    }
    ear.start_recording().unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if told.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "no recording ever came back");
        thread::sleep(Duration::from_millis(2));
    }
    let (samples, counted) = told
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expect("the loop only ends with an answer");
    ear.destroy();

    let counted_samples = (counted.as_secs_f64() * RATE) as usize;
    samples.saturating_sub(counted_samples)
}

#[test]
fn nothing_is_added_when_none_is_asked_for() {
    let extra = extra_samples(Duration::ZERO, Duration::from_millis(600));
    assert_eq!(
        extra, 0,
        "audio appeared in front of a recording that wanted none"
    );
}

#[test]
fn audio_from_before_the_open_is_put_in_front() {
    let wanted = Duration::from_millis(500);
    let extra = extra_samples(wanted, Duration::from_millis(900));

    let asked = (wanted.as_secs_f64() * RATE) as usize;
    // Whole chunks come back, so a little over is right and under is not.
    assert!(
        (asked..asked + 2048).contains(&extra),
        "asked for {asked} samples of history and got {extra}"
    );
}

/// A history shorter than the request gives what exists, not an error.
#[test]
fn asking_for_more_history_than_exists_gives_what_there_is() {
    let wanted = Duration::from_millis(1500);
    let extra = extra_samples(wanted, Duration::from_millis(300));

    let asked = (wanted.as_secs_f64() * RATE) as usize;
    assert!(extra > 0, "no history at all came back");
    assert!(
        extra < asked,
        "got {extra} samples of history from a run only 300 ms old"
    );
}

/// More pre-roll than the history can hold is a settings error, caught
/// when it is set rather than silently trimmed later.
#[test]
fn asking_for_more_than_the_history_holds_is_refused() {
    let ear = ear();
    let err = ear
        .set_pre_roll(Duration::from_secs(30))
        .expect_err("30 s of history is more than the queue keeps");
    assert!(err.to_string().contains("pre-roll"), "{err}");
}
