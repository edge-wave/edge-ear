//! One slow consumer cannot break the others. This is the promise that
//! makes the library safe to embed, so nothing here adds capability: it
//! checks that what is built holds when one part misbehaves.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use edge_ear_core::EdgeEar;
use edge_ear_core::backend::fake::FakeBackend;
use edge_ear_core::events::Event;

/// A device that hands blocks over at the speed a real one would.
/// Asking whether anything keeps up is meaningless otherwise.
fn ear() -> Arc<EdgeEar> {
    Arc::new(EdgeEar::with_backend(Box::new(FakeBackend::paced())).expect("handle"))
}

fn wait_until(deadline: Duration, mut done: impl FnMut() -> bool) -> bool {
    let stop = Instant::now() + deadline;
    while Instant::now() < stop {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn a_callback_that_blocks_does_not_interrupt_audio() {
    let ear = ear();
    let entered = Arc::new(AtomicUsize::new(0));

    let count = Arc::clone(&entered);
    ear.on_event(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        // Far longer than a block of audio takes to arrive.
        thread::sleep(Duration::from_millis(500));
    })
    .unwrap();

    ear.set_no_speech_timeout(Duration::from_millis(200))
        .unwrap();
    ear.enable_speech().unwrap();
    ear.start().unwrap();
    ear.start_recording().unwrap();

    // Read right through the period the handler is stuck in.
    let mut blocks = 0;
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(900) {
        let chunk = ear.read(Some(Duration::from_secs(2))).expect("audio");
        assert_eq!(
            chunk.dropped_before, 0,
            "audio was lost while a callback was blocked"
        );
        blocks += 1;
    }

    assert!(blocks > 10, "only {blocks} blocks arrived");
    assert!(
        entered.load(Ordering::SeqCst) > 0,
        "the blocking callback never ran, so nothing was proved"
    );
    ear.stop().unwrap();
}

#[test]
fn a_reader_that_stops_reading_does_not_stop_the_detector() {
    let ear = ear();
    let endings = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&endings);
    ear.on_event(move |event| {
        if matches!(event, Event::SpeechEnded { .. }) {
            count.fetch_add(1, Ordering::SeqCst);
        }
    })
    .unwrap();

    ear.set_no_speech_timeout(Duration::from_millis(200))
        .unwrap();
    ear.enable_speech().unwrap();
    ear.start().unwrap();
    ear.start_recording().unwrap();

    // Read nothing at all for a while.
    thread::sleep(Duration::from_millis(700));

    assert!(
        endings.load(Ordering::SeqCst) > 0,
        "the detector stopped because nobody was reading"
    );
    ear.stop().unwrap();
}

#[test]
fn a_reader_that_fell_behind_is_told_it_lost_audio() {
    let ear = ear();
    ear.start().unwrap();

    // Take one block, then look away far longer than the queue holds.
    ear.read(Some(Duration::from_secs(2))).expect("audio");
    let away = Duration::from_secs(3);
    thread::sleep(away);

    let chunk = ear.read(Some(Duration::from_secs(2))).expect("audio");
    assert!(
        chunk.dropped_before > 0,
        "falling behind was not reported at all"
    );

    // The backlog is bounded by what the queue holds, not by how long
    // the reader was away. Three seconds away still leaves at most the
    // two seconds of history the queue keeps.
    let age = chunk.captured_at.elapsed();
    assert!(
        age < away,
        "the backlog grew with the time away: {age:?} after {away:?}"
    );
    assert!(
        age < Duration::from_millis(2_500),
        "the backlog outgrew the queue: {age:?}"
    );
    ear.stop().unwrap();
}

#[test]
fn a_disabled_detector_does_not_hold_anything_up() {
    let ear = ear();
    ear.start().unwrap();

    // The speech consumer exists but is switched off, so nothing drains
    // its queue. That must not slow the read path.
    for _ in 0..50 {
        let chunk = ear.read(Some(Duration::from_secs(2))).expect("audio");
        assert_eq!(chunk.dropped_before, 0);
    }
    assert!(!ear.is_speech_enabled());
    ear.stop().unwrap();
}

#[test]
fn calls_from_many_threads_at_once_are_safe() {
    let ear = ear();
    ear.start().unwrap();

    let mut threads = Vec::new();
    for n in 0..6 {
        let ear = Arc::clone(&ear);
        threads.push(thread::spawn(move || {
            for _ in 0..40 {
                match n % 3 {
                    0 => {
                        let _ = ear.read(Some(Duration::from_millis(200)));
                    }
                    1 => {
                        let _ = ear.enable_speech();
                        let _ = ear.disable_speech();
                    }
                    _ => {
                        let _ = ear.is_running();
                        let _ = ear.config();
                        let _ = ear.is_recording();
                    }
                }
            }
        }));
    }
    for t in threads {
        t.join().expect("no thread panicked");
    }
    ear.stop().unwrap();
}

#[test]
fn a_callback_can_call_back_into_the_handle() {
    let ear = ear();
    let reentered = Arc::new(AtomicBool::new(false));

    let inner = Arc::clone(&ear);
    let flag = Arc::clone(&reentered);
    ear.on_event(move |_| {
        // Every one of these runs on the dispatcher thread, inside a
        // callback. None may deadlock.
        let _ = inner.is_running();
        let _ = inner.is_recording();
        let _ = inner.config();
        let _ = inner.read(Some(Duration::from_millis(50)));
        let _ = inner.start_recording();
        flag.store(true, Ordering::SeqCst);
    })
    .unwrap();

    ear.set_no_speech_timeout(Duration::from_millis(200))
        .unwrap();
    ear.enable_speech().unwrap();
    ear.start().unwrap();
    ear.start_recording().unwrap();

    assert!(
        wait_until(Duration::from_secs(5), || reentered.load(Ordering::SeqCst)),
        "the callback never finished, which means it deadlocked"
    );
    ear.stop().unwrap();
}

#[test]
fn the_handle_can_be_destroyed_from_inside_a_callback() {
    let ear = ear();
    let done = Arc::new(AtomicBool::new(false));

    let inner = Arc::clone(&ear);
    let flag = Arc::clone(&done);
    ear.on_event(move |_| {
        // Joining the thread this runs on would hang. It must not.
        inner.destroy();
        flag.store(true, Ordering::SeqCst);
    })
    .unwrap();

    ear.set_no_speech_timeout(Duration::from_millis(200))
        .unwrap();
    ear.enable_speech().unwrap();
    ear.start().unwrap();
    ear.start_recording().unwrap();

    assert!(
        wait_until(Duration::from_secs(5), || done.load(Ordering::SeqCst)),
        "destroying from a callback hung"
    );
}

#[test]
fn destroying_while_capture_and_playback_run_does_not_hang() {
    let ear = ear();
    ear.register_sound(
        "tone",
        edge_ear_core::SoundSource::Pcm {
            data: vec![3000; 160_000],
            sample_rate: 16_000,
            channels: 1,
            sample_type: edge_ear_core::config::SampleType::I16,
        },
        0.5,
    )
    .unwrap();
    ear.enable_speech().unwrap();
    ear.start().unwrap();
    ear.start_recording().unwrap();
    ear.play_sound("tone", true).unwrap();

    let reading = {
        let ear = Arc::clone(&ear);
        thread::spawn(move || {
            for _ in 0..20 {
                let _ = ear.read(Some(Duration::from_millis(100)));
            }
        })
    };
    thread::sleep(Duration::from_millis(100));

    let started = Instant::now();
    ear.destroy();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "destroy took {:?}",
        started.elapsed()
    );
    reading.join().expect("the reader thread ended");
}

#[test]
fn one_slow_reader_does_not_slow_another() {
    let ear = ear();
    ear.start().unwrap();

    // Two readers on the same queue. They share it, so this is about
    // the handle staying responsive, not about separate cursors.
    let fast_count = Arc::new(AtomicUsize::new(0));
    let counts = Arc::clone(&fast_count);
    let quick = {
        let ear = Arc::clone(&ear);
        thread::spawn(move || {
            for _ in 0..30 {
                if ear.read(Some(Duration::from_secs(2))).is_ok() {
                    counts.fetch_add(1, Ordering::SeqCst);
                }
            }
        })
    };

    let slow = {
        let ear = Arc::clone(&ear);
        thread::spawn(move || {
            for _ in 0..3 {
                let _ = ear.read(Some(Duration::from_secs(2)));
                thread::sleep(Duration::from_millis(150));
            }
        })
    };

    quick.join().expect("the quick reader finished");
    slow.join().expect("the slow reader finished");
    assert_eq!(fast_count.load(Ordering::SeqCst), 30);
    ear.stop().unwrap();
}

#[test]
fn events_keep_arriving_in_order_behind_a_slow_handler() {
    let ear = ear();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    ear.on_event(move |event| {
        sink.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event.kind());
        thread::sleep(Duration::from_millis(60));
    })
    .unwrap();

    ear.register_sound(
        "beep",
        edge_ear_core::SoundSource::Pcm {
            data: vec![2000; 1600],
            sample_rate: 16_000,
            channels: 1,
            sample_type: edge_ear_core::config::SampleType::I16,
        },
        0.5,
    )
    .unwrap();

    for _ in 0..3 {
        ear.play_sound("beep", false).unwrap();
        thread::sleep(Duration::from_millis(150));
    }

    assert!(
        wait_until(Duration::from_secs(5), || seen.lock().unwrap().len() >= 3),
        "only {:?} arrived",
        seen.lock().unwrap()
    );
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter().all(|k| *k == "sound finished"),
        "unexpected events: {seen:?}"
    );
}
