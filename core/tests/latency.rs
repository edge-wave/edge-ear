//! How long the library takes to pass word of something along: the gap
//! between the moment it knows and the moment the application is told.
//! Run with `--nocapture` to see the numbers.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use edge_ear_core::EdgeEar;
use edge_ear_core::backend::fake::FakeBackend;
use edge_ear_core::events::Event;

/// A recording is measured in audio, not in wall clock, so the
/// microphone has to run at the speed a real one would for the two to
/// be comparable at all.
fn paced_ear() -> EdgeEar {
    EdgeEar::with_backend(Box::new(FakeBackend::paced())).expect("handle")
}

/// One measurement: how far behind the audio the notification arrived.
fn measure_end_of_speech(no_speech_timeout: Duration) -> Duration {
    let ear = paced_ear();
    ear.set_no_speech_timeout(no_speech_timeout).unwrap();
    ear.set_max_recording(Duration::from_secs(30)).unwrap();

    let told = Arc::new(Mutex::new(None::<(Instant, Duration)>));
    let sink = Arc::clone(&told);
    ear.on_event(move |event| {
        if let Event::SpeechEnded { duration, .. } = event {
            let mut slot = sink.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                *slot = Some((Instant::now(), duration));
            }
        }
    })
    .unwrap();

    ear.start().unwrap();
    ear.enable_speech().unwrap();
    let opened = Instant::now();
    ear.start_recording().unwrap();

    let deadline = Instant::now() + no_speech_timeout + Duration::from_secs(10);
    let answer = loop {
        if let Some(seen) = *told.lock().unwrap_or_else(|e| e.into_inner()) {
            break Some(seen);
        }
        assert!(Instant::now() < deadline, "no notification ever arrived");
        thread::sleep(Duration::from_millis(1));
    };
    ear.destroy();

    let (at, audio) = answer.expect("the loop only breaks with an answer");
    // The recording says how much audio it ended on. Anything past that
    // is the library carrying the news.
    at.duration_since(opened).saturating_sub(audio)
}

/// A notification must reach the application within 200 ms of the audio
/// that ended the recording.
#[test]
fn the_end_of_a_recording_is_passed_on_promptly() {
    let target = Duration::from_millis(200);

    // Several runs, because one can be unlucky on a busy machine.
    let mut worst = Duration::ZERO;
    for _ in 0..5 {
        worst = worst.max(measure_end_of_speech(Duration::from_millis(500)));
    }

    println!("end of recording: worst of five runs was {worst:?}");
    assert!(
        worst < target,
        "the application waited {worst:?}, longer than the {target:?} allowed"
    );
}

/// Under load the notification must still be prompt. Not about a slow
/// handler: those run one after another and necessarily hold up the
/// next. What they must not hold up is audio, checked elsewhere.
#[test]
fn reading_hard_does_not_delay_the_notification() {
    let ear = Arc::new(paced_ear());
    ear.set_no_speech_timeout(Duration::from_millis(500))
        .unwrap();

    let told = Arc::new(Mutex::new(None::<(Instant, Duration)>));
    let sink = Arc::clone(&told);
    ear.on_event(move |event| {
        if let Event::SpeechEnded { duration, .. } = event {
            let mut slot = sink.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                *slot = Some((Instant::now(), duration));
            }
        }
    })
    .unwrap();

    ear.start().unwrap();
    ear.enable_speech().unwrap();

    let reader = {
        let ear = Arc::clone(&ear);
        let told = Arc::clone(&told);
        thread::spawn(move || {
            while told.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
                let _ = ear.read(Some(Duration::from_millis(50)));
            }
        })
    };

    let opened = Instant::now();
    ear.start_recording().unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    while told.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
        assert!(Instant::now() < deadline, "no notification ever arrived");
        thread::sleep(Duration::from_millis(1));
    }
    reader.join().expect("the reader finished");

    let (at, audio) = told
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expect("the loop only ends with an answer");
    ear.destroy();

    let behind = at.duration_since(opened).saturating_sub(audio);
    println!("with a reader running flat out: {behind:?} behind the audio");
    assert!(
        behind < Duration::from_millis(200),
        "the application waited {behind:?} while another consumer was reading"
    );
}
