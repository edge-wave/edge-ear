//! When audio handed to a speaker is heard. The device callback says what
//! it took and how far ahead it runs; the player asks when a sample comes out.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::config::AudioFormat;

/// Shared by one output stream's callback, which writes, and the player,
/// which reads. Positions count interleaved samples from the stream's start.
pub struct Playout {
    epoch: Instant,
    format: AudioFormat,
    /// Samples the device has taken so far.
    taken: AtomicU64,
    /// When the last of them is heard, in nanoseconds after `epoch`.
    heard_ns: AtomicU64,
}

impl Playout {
    pub fn new(format: AudioFormat) -> Self {
        Self {
            epoch: Instant::now(),
            format,
            taken: AtomicU64::new(0),
            heard_ns: AtomicU64::new(0),
        }
    }

    /// Called by the device callback once it has taken `samples` real
    /// samples, the first of which is heard `ahead` from now.
    pub fn took(&self, samples: usize, ahead: Duration) {
        if samples == 0 {
            return;
        }
        let heard = Instant::now() + ahead + self.span(samples as u64);
        let ns = heard.saturating_duration_since(self.epoch).as_nanos();
        // The time goes first, so a reader that sees the new count also
        // sees a time at least as late as the one that belongs to it.
        self.heard_ns
            .store(u64::try_from(ns).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.taken.fetch_add(samples as u64, Ordering::Release);
    }

    /// When the sample at `position` comes out of the speaker, or `None`
    /// while the device has not taken it yet.
    pub fn heard_at(&self, position: u64) -> Option<Instant> {
        let taken = self.taken.load(Ordering::Acquire);
        if taken < position {
            return None;
        }
        let last = self.epoch + Duration::from_nanos(self.heard_ns.load(Ordering::Relaxed));
        Some(
            last.checked_sub(self.span(taken - position))
                .unwrap_or(last),
        )
    }

    /// How long `samples` interleaved samples take to play.
    fn span(&self, samples: u64) -> Duration {
        let frames = samples / u64::from(self.format.channels.max(1));
        Duration::from_secs_f64(frames as f64 / f64::from(self.format.sample_rate.max(1)))
    }
}

/// How far ahead of the speaker a callback runs when the device will not
/// say: one buffer, since what it fills now plays after what it filled last.
pub fn guessed_ahead(buffer_samples: usize, format: AudioFormat) -> Duration {
    let frames = buffer_samples / usize::from(format.channels.max(1));
    Duration::from_secs_f64(frames as f64 / f64::from(format.sample_rate.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_heard_before_the_device_takes_it() {
        let playout = Playout::new(AudioFormat::mono_16k());
        assert!(playout.heard_at(1).is_none());
        playout.took(1_600, Duration::ZERO);
        assert!(playout.heard_at(1_600).is_some());
        assert!(playout.heard_at(1_601).is_none());
    }

    #[test]
    fn a_sample_is_heard_after_the_device_delay_and_those_before_it() {
        let playout = Playout::new(AudioFormat::mono_16k());
        let before = Instant::now();
        // A tenth of a second of audio, a fifth of a second ahead.
        playout.took(1_600, Duration::from_millis(200));

        let end = playout.heard_at(1_600).expect("taken");
        let middle = playout.heard_at(800).expect("taken");
        assert!(end >= before + Duration::from_millis(300));
        assert!(end < Instant::now() + Duration::from_millis(300));
        assert_eq!(end - middle, Duration::from_millis(50));
    }

    #[test]
    fn stereo_counts_frames_not_samples() {
        let playout = Playout::new(AudioFormat::new(48_000, 2, crate::config::SampleType::F32));
        playout.took(9_600, Duration::ZERO);
        let end = playout.heard_at(9_600).expect("taken");
        let start = playout.heard_at(0).expect("taken");
        assert_eq!(end - start, Duration::from_millis(100));
    }
}
