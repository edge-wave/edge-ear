//! Listens on the real microphone and prints what it hears.
//!
//! Shows every path at once: a level meter drawn from the raw read
//! path, and a line whenever the wake word is heard, the alert
//! finishes, or the detector decides speech has ended. One being busy
//! never stops the others, which is the point.
//!
//! Run `cargo run --example listen -- help` for the arguments. The
//! bare `--` is where cargo stops reading arguments for itself and
//! passes the rest along.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use edge_ear_core::EdgeEar;
use edge_ear_core::SoundSource;
use edge_ear_core::config::{AudioFormat, SampleType, Target};
use edge_ear_core::events::Event;

const HELP: &str = "\
listen — hear what the library hears

  cargo run --example listen -- [seconds] [device]

  seconds   how long to run. 0 or `forever` runs until Ctrl-C.
            default 20.
  device    an identifier from the listing. default is the system one.

  help      this text
  list      show the devices

Set these to wait for a wake word rather than recording at once:

  EDGE_EAR_WAKE_DIR        directory holding the models. Needs
                           melspectrogram.onnx, embedding_model.onnx,
                           and a wake word. This library ships none.
  EDGE_EAR_WAKE_WORD       which wake word file to use.
                           default hey_jarvis_v0.1.onnx
  EDGE_EAR_WAKE_THRESHOLD  how sure it must be, 0.0 to 1.0.
                           lower catches more and mishears more.
                           default 0.5
  EDGE_EAR_SILENCE         seconds of quiet that end a recording.
                           default 0.8
  EDGE_EAR_WAKE_SETTLE     frames of 80 ms to look away for after
                           hearing it, so the same words are not heard
                           twice on their way out. default 20

The wake score is shown beside the level meter as it is heard, whether
or not it reached the threshold. Watch it while saying the wake word to
see how close the model comes.

For example:

  EDGE_EAR_WAKE_DIR=~/models EDGE_EAR_WAKE_THRESHOLD=0.35 \\
    cargo run --example listen -- forever
";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let first = std::env::args().nth(1);

    if matches!(first.as_deref(), Some("help" | "-h" | "--help")) {
        print!("{HELP}");
        return Ok(());
    }

    let ear = Arc::new(EdgeEar::new()?);

    if first.as_deref() == Some("list") {
        for d in ear.input_devices()? {
            let mark = if d.is_default { " (default)" } else { "" };
            println!("{:<40} {}{mark}", d.id, d.name);
        }
        return Ok(());
    }

    // Nothing to run out is written as no limit at all, rather than a
    // very large number that would still end one day.
    let limit: Option<Duration> = match first.as_deref() {
        Some("forever" | "0") => None,
        Some(text) => Some(Duration::from_secs(text.parse().unwrap_or(20))),
        None => Some(Duration::from_secs(20)),
    };
    if let Some(id) = std::env::args().nth(2) {
        ear.set_input_device(Some(&id))?;
        println!("microphone: {id}");
    } else {
        match ear.input_devices()?.iter().find(|d| d.is_default) {
            Some(d) => println!("microphone: {} [{}]", d.name, d.id),
            None => println!("microphone: whatever opens"),
        }
    }

    ear.set_format(Target::Read, AudioFormat::mono_16k())?;
    ear.set_silence_duration(env_seconds("EDGE_EAR_SILENCE", 0.8))?;
    ear.set_no_speech_timeout(Duration::from_secs(6))?;
    ear.set_max_recording(Duration::from_secs(15))?;

    // A short rising beep, so there is no sound file to find.
    ear.register_sound("beep", beep(), 0.4)?;

    let waiting_for_wake = set_up_wake(&ear)?;

    let started = Instant::now();
    let opener = Arc::clone(&ear);
    ear.on_event(move |event| {
        let at = started.elapsed().as_secs_f64();
        match event {
            Event::WakeDetected { score } => {
                println!("\r  [{at:>5.1}s] heard the wake word ({score:.3}) — listening");
            }
            Event::SoundFinished { id } => {
                println!("\r  [{at:>5.1}s] {id} finished, now counting silence");
            }
            Event::SpeechEnded {
                audio,
                reason,
                duration,
                sample_rate,
            } => {
                println!(
                    "\r  [{at:>5.1}s] speech ended: {reason} after {:.1}s, \
                     {:.1}s of audio",
                    duration.as_secs_f64(),
                    audio.len() as f64 / sample_rate as f64
                );
                if waiting_for_wake {
                    println!("           say it again when you like");
                } else {
                    let _ = opener.start_recording();
                }
            }
            Event::DeviceError { device, message } => {
                println!("\r  [{at:>5.1}s] {device} device problem: {message}");
            }
            Event::EventsDropped { count } => {
                println!("\r  [{at:>5.1}s] {count} notifications were dropped");
            }
        }
    })?;

    ear.enable_speech()?;
    ear.start()?;

    let how_long = match limit {
        Some(d) => format!("for {}s", d.as_secs()),
        None => "until you press Ctrl-C".to_string(),
    };
    if waiting_for_wake {
        println!("\nlistening {how_long}. Say the wake word, then your request.");
    } else {
        ear.start_recording()?;
        println!("\nlistening {how_long}. Say something, then stop.");
    }
    println!("the bar is loudness from the raw read path, which runs whatever else is going on.\n");

    let deadline = limit.map(|d| Instant::now() + d);
    let mut last_drawn = Instant::now();
    let mut loudest = 0.0f64;
    while deadline.is_none_or(|end| Instant::now() < end) {
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
        if last_drawn.elapsed() >= Duration::from_millis(120)
            && let Some(samples) = chunk.samples.as_i16()
        {
            last_drawn = Instant::now();
            loudest = loudest.max(rms(samples));
            // The score is shown whether or not it counted, so a wake
            // word that nearly made it can be told from one the model
            // never noticed at all.
            let score = match ear.wake_score() {
                Some(score) => format!("  wake {}", score_bar(score)),
                None if waiting_for_wake => "  wake  listening".to_string(),
                None => String::new(),
            };
            print!("\r  {}{score}", meter(samples));
            let _ = std::io::stdout().flush();
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

/// Load the wake word models if a directory was given. Returns whether
/// the run is waiting to be called rather than recording at once.
fn set_up_wake(ear: &EdgeEar) -> Result<bool, Box<dyn std::error::Error>> {
    let Ok(dir) = std::env::var("EDGE_EAR_WAKE_DIR") else {
        println!("no wake word models given, so this records straight away.");
        println!("set EDGE_EAR_WAKE_DIR to wait for a wake word instead.");
        return Ok(false);
    };
    let dir = PathBuf::from(dir);
    let word =
        std::env::var("EDGE_EAR_WAKE_WORD").unwrap_or_else(|_| "hey_jarvis_v0.1.onnx".to_string());

    ear.load_wake_features(
        &dir.join("melspectrogram.onnx"),
        &dir.join("embedding_model.onnx"),
    )?;
    ear.load_wake_model(&dir.join(&word))?;

    if let Ok(text) = std::env::var("EDGE_EAR_WAKE_SETTLE") {
        let frames: u32 = text
            .parse()
            .map_err(|_| format!("EDGE_EAR_WAKE_SETTLE must be a whole number, got {text:?}"))?;
        ear.set_wake_settle_frames(frames)?;
    }
    if let Ok(text) = std::env::var("EDGE_EAR_WAKE_THRESHOLD") {
        let value: f32 = text
            .parse()
            .map_err(|_| format!("EDGE_EAR_WAKE_THRESHOLD must be a number, got {text:?}"))?;
        ear.set_wake_threshold(value)?;
    }
    ear.enable_wake(Some("beep"))?;

    println!("wake word: {word}");
    let settle = ear.config().tunable.wake_settle_frames;
    println!("threshold: {:.2}", ear.config().tunable.wake_threshold);
    println!(
        "after hearing it, looks away for {settle} frames ({:.1}s)",
        settle as f64 * 0.08
    );
    Ok(true)
}

/// A setting read from the environment, in seconds.
fn env_seconds(name: &str, fallback: f64) -> Duration {
    let seconds = std::env::var(name)
        .ok()
        .and_then(|text| text.parse::<f64>().ok())
        .filter(|value| *value > 0.0)
        .unwrap_or(fallback);
    Duration::from_secs_f64(seconds)
}

/// A short rising tone, faded by the player at both ends.
fn beep() -> SoundSource {
    let rate = 16_000.0;
    let data = (0..3200)
        .map(|n| {
            let t = n as f32 / rate;
            let hz = 660.0 + 440.0 * t / 0.2;
            ((t * hz * std::f32::consts::TAU).sin() * 7000.0) as i16
        })
        .collect();
    SoundSource::Pcm {
        data,
        sample_rate: 16_000,
        channels: 1,
        sample_type: SampleType::I16,
    }
}

fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|s| (*s as f64).powi(2)).sum();
    (sum / samples.len() as f64).sqrt()
}

/// The wake word score, with a mark at how sure it must be.
fn score_bar(score: f32) -> String {
    let width = 12;
    let filled = ((score.clamp(0.0, 1.0)) * width as f32) as usize;
    format!(
        "{:.3} [{}{}]",
        score,
        "=".repeat(filled),
        " ".repeat(width - filled)
    )
}

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
