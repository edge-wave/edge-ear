//! Listens on the real microphone and prints what it hears.
//!
//! Shows both paths at once: a level meter drawn from the raw read
//! path, and a line whenever the detector decides speech has ended.
//! One being busy never stops the other, which is the point.
//!
//!     cargo run --example listen
//!     cargo run --example listen -- 30              # seconds, default 20
//!     cargo run --example listen -- 30 <device-id>  # a device from the list
//!     cargo run --example listen -- list            # show the devices

use std::sync::Arc;
use std::time::{Duration, Instant};

use edge_ear_core::EdgeEar;
use edge_ear_core::config::{AudioFormat, Target};
use edge_ear_core::events::Event;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let first = std::env::args().nth(1);
    let ear = Arc::new(EdgeEar::new()?);

    if first.as_deref() == Some("list") {
        for d in ear.input_devices()? {
            let mark = if d.is_default { " (default)" } else { "" };
            println!("{:<40} {}{mark}", d.id, d.name);
        }
        return Ok(());
    }

    let seconds: u64 = first.and_then(|a| a.parse().ok()).unwrap_or(20);
    let wanted = std::env::args().nth(2);

    if let Some(id) = wanted.as_deref() {
        ear.set_input_device(Some(id))?;
        println!("microphone: {id}");
    } else {
        let devices = ear.input_devices()?;
        match devices.iter().find(|d| d.is_default) {
            Some(d) => println!("microphone: {} [{}]", d.name, d.id),
            None => println!("microphone: none marked default, using whatever opens"),
        }
    }

    ear.set_format(Target::Read, AudioFormat::mono_16k())?;
    // Short enough that something happens while you watch.
    ear.set_silence_duration(Duration::from_millis(800))?;
    ear.set_no_speech_timeout(Duration::from_secs(6))?;
    ear.set_max_recording(Duration::from_secs(15))?;

    let started = Instant::now();
    let opener = Arc::clone(&ear);
    ear.on_event(move |event| match event {
        Event::SpeechEnded {
            audio,
            reason,
            duration,
            sample_rate,
        } => {
            let seconds = audio.len() as f64 / sample_rate as f64;
            println!(
                "\r  [{:>5.1}s] speech ended: {reason} after {:.1}s, {} samples ({:.1}s of audio)",
                started.elapsed().as_secs_f64(),
                duration.as_secs_f64(),
                audio.len(),
                seconds
            );
            // Straight into the next one, so you can keep talking.
            let _ = opener.start_recording();
        }
        Event::DeviceError { device, message } => {
            println!("\r  {device} device problem: {message}");
        }
        Event::EventsDropped { count } => {
            println!("\r  {count} notifications were dropped");
        }
        _ => {}
    })?;

    ear.enable_speech()?;
    ear.start()?;
    ear.start_recording()?;

    println!(
        "listening for {seconds}s. Say something, then stop, and watch for the line.\n\
         the bar is loudness from the raw read path, which runs whatever the detector is doing.\n"
    );

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut last_drawn = Instant::now();
    let mut loudest = 0.0f64;
    while Instant::now() < deadline {
        let chunk = match ear.read(Some(Duration::from_millis(500))) {
            Ok(chunk) => chunk,
            Err(e) => {
                println!("\r  read stopped: {e}");
                break;
            }
        };

        if chunk.dropped_before > 0 {
            println!(
                "\r  the meter fell behind and lost {} blocks",
                chunk.dropped_before
            );
        }

        // Redraw a few times a second rather than per block.
        if last_drawn.elapsed() >= Duration::from_millis(120) {
            last_drawn = Instant::now();
            if let Some(samples) = chunk.samples.as_i16() {
                loudest = loudest.max(rms(samples));
                print!("\r  {}", meter(samples));
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
        }
    }

    println!("\n");
    if loudest == 0.0 {
        println!("every block was digital silence.");
        println!("the microphone is muted, or there is no audio session here.");
        println!("try: cargo run --example listen -- list");
    } else {
        println!("loudest block: rms {loudest:.0}");
    }
    ear.stop()?;
    Ok(())
}

fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|s| (*s as f64).powi(2)).sum();
    (sum / samples.len() as f64).sqrt()
}

/// A loudness bar, so there is something to watch while talking.
fn meter(samples: &[i16]) -> String {
    let rms = rms(samples);
    let width = 40;
    let filled = ((rms / 3000.0).min(1.0) * width as f64) as usize;
    format!(
        "[{}{}] rms {rms:>7.0}",
        "#".repeat(filled),
        " ".repeat(width - filled)
    )
}
