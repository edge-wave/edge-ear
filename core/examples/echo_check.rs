//! Plays noise through the real speaker and measures how much of it the microphone hears back,
//! with echo cancellation off and then on. Run with `-- help` for the arguments.

use std::time::Duration;

use edge_ear_core::echo::BuiltinCanceller;
use edge_ear_core::{EdgeEar, Samples, SoundSource};

const HELP: &str = "\
echo_check — measure how much echo is taken out

  cargo run --release --features webrtc-aec --example echo_check -- [seconds] [volume]

  seconds   how long the noise plays each time. default 6.
  volume    how loud, 0.0 to 1.0. default 0.3.

  help      this text

It listens to the room in silence first, then plays the same noise with
echo cancellation off and on, and prints the microphone level for each
second. The canceller takes a second or two to find the echo path, so
the summary leaves the first two seconds out. Keep the room quiet.
";

const RATE: usize = 16_000;
/// Seconds the canceller is given to find the echo path before it is judged.
const SETTLE: usize = 2;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "help") {
        print!("{HELP}");
        return Ok(());
    }
    let seconds: usize = args.first().map_or(Ok(6), |a| a.parse())?;
    let volume: f32 = args.get(1).map_or(Ok(0.3), |a| a.parse())?;
    if seconds <= SETTLE {
        return Err(format!("play for more than {SETTLE} seconds").into());
    }

    let floor = listen(false, seconds, None)?;
    let off = listen(false, seconds, Some(volume))?;
    let on = listen(true, seconds, Some(volume))?;

    println!("second   room   off    on   (dBFS)");
    for s in 0..seconds {
        println!("{s:>6} {:>6.1} {:>5.1} {:>5.1}", floor[s], off[s], on[s]);
    }
    let judged = |levels: &[f64]| average_db(&levels[SETTLE..]);
    let (floor, off, on) = (judged(&floor), judged(&off), judged(&on));
    println!();
    println!("from second {SETTLE} on: room {floor:.1}, off {off:.1}, on {on:.1} dBFS");
    println!("echo taken out: {:.1} dB", off - on);
    println!("echo left above the room: {:.1} dB", on - floor);
    Ok(())
}

/// The microphone level for each second, while noise plays at `volume` or nothing plays at all.
fn listen(
    cancel: bool,
    seconds: usize,
    volume: Option<f32>,
) -> edge_ear_core::error::Result<Vec<f64>> {
    let ear = EdgeEar::new()?;
    ear.set_echo_canceller(if cancel {
        BuiltinCanceller::Webrtc
    } else {
        BuiltinCanceller::Off
    })?;
    ear.start()?;
    if let Some(volume) = volume {
        ear.register_sound("noise", noise(seconds), volume)?;
        ear.play_sound("noise", false)?;
    }

    let mut levels = Vec::with_capacity(seconds);
    let (mut energy, mut count) = (0.0, 0);
    while levels.len() < seconds {
        let chunk = ear.read(Some(Duration::from_secs(2)))?;
        for sample in chunk.samples.as_i16().unwrap_or_default() {
            energy += (f64::from(*sample) / 32_768.0).powi(2);
            count += 1;
            if count == RATE {
                levels.push(10.0 * (energy / RATE as f64).max(1e-12).log10());
                (energy, count) = (0.0, 0);
            }
        }
    }
    ear.stop()?;
    ear.destroy();
    // Give the devices a moment to be let go before the next run opens them.
    std::thread::sleep(Duration::from_millis(300));
    Ok(levels)
}

fn noise(seconds: usize) -> SoundSource {
    let mut seed = 1u32;
    let samples = (0..seconds * RATE)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 16) as i16 / 2
        })
        .collect();
    SoundSource::Pcm {
        data: Samples::I16(samples),
        sample_rate: RATE as u32,
        channels: 1,
    }
}

fn average_db(levels: &[f64]) -> f64 {
    let power: f64 = levels.iter().map(|db| 10f64.powf(db / 10.0)).sum();
    10.0 * (power / levels.len() as f64).log10()
}
