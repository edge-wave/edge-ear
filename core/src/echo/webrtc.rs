//! WebRTC's echo canceller (AEC3), built from source by the `webrtc-audio-processing` crate.

use webrtc_audio_processing::Processor;
use webrtc_audio_processing::config::{Config, EchoCanceller as Aec};

use super::EchoCanceller;
use crate::config::Device;
use crate::error::{Error, Result};

/// The rates AEC3 runs at natively. Anything else would be resampled inside it anyway.
const RATES: [u32; 4] = [8_000, 16_000, 32_000, 48_000];

/// AEC3 with its own delay estimator, and nothing else of WebRTC's processing switched on.
pub struct WebrtcCanceller {
    processor: Processor,
    sample_rate: u32,
    /// A failure is logged when it starts, not on every frame.
    failing: bool,
}

impl WebrtcCanceller {
    /// Run at the lowest native rate that keeps everything up to `wanted` Hz.
    pub fn new(wanted: u32) -> Result<Self> {
        let sample_rate = RATES.into_iter().find(|r| *r >= wanted).unwrap_or(48_000);
        let processor = Processor::new(sample_rate).map_err(|e| Error::Backend {
            device: Device::Input,
            reason: format!("the echo canceller would not start: {e}"),
        })?;
        // Leaving the delay unset is what lets AEC3 find it on its own.
        processor.set_config(Config {
            echo_canceller: Some(Aec::Full {
                stream_delay_ms: None,
            }),
            ..Default::default()
        });
        Ok(Self {
            processor,
            sample_rate,
            failing: false,
        })
    }

    fn note(
        &mut self,
        what: &str,
        result: std::result::Result<(), webrtc_audio_processing::Error>,
    ) {
        match result {
            Err(e) if !self.failing => {
                log::error!("the echo canceller could not take a {what} frame: {e}");
                self.failing = true;
            }
            Err(_) => {}
            Ok(()) => self.failing = false,
        }
    }
}

impl EchoCanceller for WebrtcCanceller {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn render(&mut self, frame: &[f32]) {
        let result = self.processor.analyze_render_frame([frame]);
        self.note("speaker", result);
    }

    fn capture(&mut self, frame: &mut [f32]) {
        let result = self.processor.process_capture_frame([frame]);
        self.note("microphone", result);
    }

    fn reset(&mut self) {
        self.processor.reinitialize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn energy(samples: &[f32]) -> f64 {
        samples.iter().map(|s| f64::from(*s).powi(2)).sum()
    }

    #[test]
    fn a_rate_between_native_ones_rounds_up() {
        assert_eq!(WebrtcCanceller::new(16_000).unwrap().sample_rate(), 16_000);
        assert_eq!(WebrtcCanceller::new(22_050).unwrap().sample_rate(), 32_000);
        assert_eq!(WebrtcCanceller::new(96_000).unwrap().sample_rate(), 48_000);
    }

    #[test]
    fn a_delayed_echo_is_taken_out() {
        let mut canceller = WebrtcCanceller::new(16_000).unwrap();
        let frame = 160;
        let delay = 6 * frame;
        let mut seed = 1u32;
        let speaker: Vec<f32> = (0..frame * 600)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect();

        let (mut before, mut after) = (0.0, 0.0);
        for (n, played) in speaker.chunks_exact(frame).enumerate() {
            canceller.render(played);
            let start = n * frame;
            let mut heard: Vec<f32> = (start..start + frame)
                .map(|i| i.checked_sub(delay).map_or(0.0, |j| speaker[j] * 0.3))
                .collect();
            let echo = energy(&heard);
            canceller.capture(&mut heard);
            // Judged once it has had four seconds to find the delay.
            if n >= 400 {
                before += echo;
                after += energy(&heard);
            }
        }
        let reduction_db = 10.0 * (before / after.max(1e-12)).log10();
        assert!(
            reduction_db > 10.0,
            "only {reduction_db:.1} dB of echo was taken out"
        );
    }
}
