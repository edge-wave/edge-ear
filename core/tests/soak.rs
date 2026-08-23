//! A long run, to catch what a short one cannot.
//!
//! Ignored by default because it takes an hour. Set `EDGE_EAR_SOAK_SECS`
//! to try a shorter one first.
//!
//! ```text
//! cargo test -p edge-ear-core --test soak -- --ignored --nocapture
//! ```

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use edge_ear_core::EdgeEar;
use edge_ear_core::backend::fake::FakeBackend;
use edge_ear_core::events::Event;
use edge_ear_core::{SoundSource, config::SampleType};

fn how_long() -> Duration {
    let secs = std::env::var("EDGE_EAR_SOAK_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600);
    Duration::from_secs(secs)
}

/// Resident memory in kilobytes, straight from the kernel.
fn memory_kb() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    pages * 4
}

#[test]
#[ignore = "takes an hour"]
fn an_hour_of_audio_with_no_gaps_and_flat_memory() {
    let run_for = how_long();
    let ear = Arc::new(EdgeEar::with_backend(Box::new(FakeBackend::paced())).expect("handle"));
    ear.set_no_speech_timeout(Duration::from_secs(2)).unwrap();
    ear.set_max_recording(Duration::from_secs(5)).unwrap();

    let recordings = Arc::new(AtomicU64::new(0));
    let sounds = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&recordings);
    let played = Arc::clone(&sounds);
    ear.on_event(move |event| match event {
        Event::SpeechEnded { .. } => {
            counted.fetch_add(1, Ordering::Relaxed);
        }
        Event::SoundFinished { .. } => {
            played.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    })
    .unwrap();

    // A short tone, so playback runs alongside everything else.
    ear.register_sound(
        "beep",
        SoundSource::Pcm {
            data: (0..3200)
                .map(|i| ((i as f32 * 0.2).sin() * 6000.0) as i16)
                .collect(),
            sample_rate: 16_000,
            channels: 1,
            sample_type: SampleType::I16,
        },
        0.5,
    )
    .unwrap();

    ear.start().unwrap();
    ear.enable_speech().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let gaps = Arc::new(AtomicU64::new(0));
    let blocks = Arc::new(AtomicU64::new(0));

    // One reader, running the whole way, watching for lost audio.
    let reader = {
        let ear = Arc::clone(&ear);
        let stop = Arc::clone(&stop);
        let gaps = Arc::clone(&gaps);
        let blocks = Arc::clone(&blocks);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match ear.read(Some(Duration::from_millis(500))) {
                    Ok(chunk) => {
                        blocks.fetch_add(1, Ordering::Relaxed);
                        gaps.fetch_add(chunk.dropped_before, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
            }
        })
    };

    let samples = Arc::new(Mutex::new(Vec::<(Duration, u64)>::new()));
    // Enough points to see a trend, whatever the run length.
    let sample_every = (run_for / 40).clamp(Duration::from_secs(1), Duration::from_secs(60));
    let cycle_every = (run_for / 200).clamp(Duration::from_secs(4), Duration::from_secs(8));

    let started = Instant::now();
    let mut next_cycle = Instant::now();
    let mut next_sample = Instant::now();

    while started.elapsed() < run_for {
        let now = Instant::now();
        if now >= next_cycle {
            // Wake, record, and play, over and over.
            let _ = ear.reset_wake();
            let _ = ear.start_recording();
            let _ = ear.play_sound("beep", false);
            next_cycle = now + cycle_every;
        }
        if now >= next_sample {
            samples
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((started.elapsed(), memory_kb()));
            next_sample = now + sample_every;
        }
        thread::sleep(Duration::from_millis(50));
    }

    stop.store(true, Ordering::Relaxed);
    reader.join().expect("the reader finished");
    ear.destroy();

    let samples = samples.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let blocks = blocks.load(Ordering::Relaxed);
    let gaps = gaps.load(Ordering::Relaxed);

    println!("ran for {:?}", started.elapsed());
    println!("blocks read: {blocks}, chunks lost: {gaps}");
    println!("recordings ended: {}", recordings.load(Ordering::Relaxed));
    println!("sounds played: {}", sounds.load(Ordering::Relaxed));

    assert!(blocks > 0, "no audio was read at all");
    assert_eq!(gaps, 0, "audio was lost during the run");

    // Memory is compared after a settling period, because the first
    // minute is startup rather than steady state.
    assert!(samples.len() >= 4, "not enough memory samples to judge");
    let settled = samples.len() / 4;
    let early = samples[settled].1;
    let late = samples[samples.len() - 1].1;
    println!("memory: {early} kB after settling, {late} kB at the end");

    let growth = late.saturating_sub(early);
    let allowed = (early / 10).max(4096);
    assert!(
        growth < allowed,
        "memory grew by {growth} kB, more than the {allowed} kB allowed"
    );
}
