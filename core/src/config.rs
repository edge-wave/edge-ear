use std::fmt;
use std::time::Duration;

use crate::error::{Error, Result};

/// Samples the wake word models need per call: 80 ms at 16 kHz.
pub const WAKE_FRAME_SAMPLES: usize = 1280;
/// Samples the speech model needs per call at 16 kHz: 32 ms.
pub const SPEECH_FRAME_SAMPLES_16K: usize = 512;
/// Samples the speech model needs per call at 8 kHz.
pub const SPEECH_FRAME_SAMPLES_8K: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SampleType {
    I16,
    F32,
}

impl fmt::Display for SampleType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SampleType::I16 => write!(f, "16-bit integer"),
            SampleType::F32 => write!(f, "32-bit float"),
        }
    }
}

/// Which consumer of live audio a setting applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    Wake,
    Speech,
    Read,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Wake => write!(f, "the wake word detector"),
            Target::Speech => write!(f, "the speech detector"),
            Target::Read => write!(f, "the read path"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Device {
    Input,
    Output,
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Device::Input => write!(f, "input"),
            Device::Output => write!(f, "output"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_type: SampleType,
}

impl AudioFormat {
    pub const fn new(sample_rate: u32, channels: u16, sample_type: SampleType) -> Self {
        Self {
            sample_rate,
            channels,
            sample_type,
        }
    }

    /// 16 kHz mono 16-bit. What both models require, and the default
    /// for the read path too.
    pub const fn mono_16k() -> Self {
        Self::new(16_000, 1, SampleType::I16)
    }

    /// Samples one model call consumes, for the targets that have a
    /// fixed frame size.
    pub fn frame_samples(&self, target: Target) -> Option<usize> {
        match target {
            Target::Wake => Some(WAKE_FRAME_SAMPLES),
            Target::Speech => match self.sample_rate {
                16_000 => Some(SPEECH_FRAME_SAMPLES_16K),
                8_000 => Some(SPEECH_FRAME_SAMPLES_8K),
                _ => None,
            },
            Target::Read => None,
        }
    }

    /// Reject a format the target cannot accept, naming the field that
    /// was wrong and what was expected. Nothing is silently adjusted.
    pub fn validate_for(&self, target: Target) -> Result<()> {
        let bad = |field: &'static str, got: String, expected: &str| {
            Err(Error::UnsupportedFormat {
                target,
                field,
                got,
                expected: expected.to_string(),
            })
        };

        match target {
            Target::Wake => {
                if self.sample_rate != 16_000 {
                    return bad("sample rate", self.sample_rate.to_string(), "16000");
                }
                if self.channels != 1 {
                    return bad("channel count", self.channels.to_string(), "1");
                }
                if self.sample_type != SampleType::I16 {
                    return bad(
                        "sample type",
                        self.sample_type.to_string(),
                        "16-bit integer",
                    );
                }
            }
            Target::Speech => {
                if self.sample_rate != 16_000 && self.sample_rate != 8_000 {
                    return bad("sample rate", self.sample_rate.to_string(), "16000 or 8000");
                }
                if self.channels != 1 {
                    return bad("channel count", self.channels.to_string(), "1");
                }
                if self.sample_type != SampleType::I16 {
                    return bad(
                        "sample type",
                        self.sample_type.to_string(),
                        "16-bit integer",
                    );
                }
            }
            Target::Read => {
                if !(8_000..=192_000).contains(&self.sample_rate) {
                    return bad(
                        "sample rate",
                        self.sample_rate.to_string(),
                        "between 8000 and 192000",
                    );
                }
                if self.channels == 0 || self.channels > 2 {
                    return bad("channel count", self.channels.to_string(), "1 or 2");
                }
            }
        }
        Ok(())
    }
}

/// Settings fixed once capture starts. Changing one afterwards is an
/// error, because the conversion pipeline is built from them.
#[derive(Debug, Clone)]
pub struct FixedConfig {
    pub wake_format: AudioFormat,
    pub speech_format: AudioFormat,
    pub read_format: AudioFormat,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub ring_capacity: Duration,
}

impl Default for FixedConfig {
    fn default() -> Self {
        Self {
            wake_format: AudioFormat::mono_16k(),
            speech_format: AudioFormat::mono_16k(),
            read_format: AudioFormat::mono_16k(),
            input_device: None,
            output_device: None,
            ring_capacity: Duration::from_secs(2),
        }
    }
}

/// Settings that may change at any time, running or not.
#[derive(Debug, Clone)]
pub struct TunableConfig {
    pub wake_threshold: f32,
    pub speech_threshold: f32,
    pub silence_duration: Duration,
    pub max_recording: Duration,
    pub no_speech_timeout: Duration,
    pub pre_roll: Duration,
    pub wake_settle_frames: u32,
}

impl Default for TunableConfig {
    fn default() -> Self {
        Self {
            wake_threshold: 0.5,
            speech_threshold: 0.5,
            silence_duration: Duration::from_secs(3),
            max_recording: Duration::from_secs(30),
            no_speech_timeout: Duration::from_secs(10),
            // Off by default. Audio from before the recording opened may
            // contain the alert sound, so turning this on is a trade the
            // application makes on purpose.
            pre_roll: Duration::ZERO,
            wake_settle_frames: 20,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub fixed: FixedConfig,
    pub tunable: TunableConfig,
}

fn check_unit(setting: &'static str, value: f32) -> Result<()> {
    if !(0.0..=1.0).contains(&value) || value.is_nan() {
        return Err(Error::InvalidValue {
            setting,
            expected: "between 0.0 and 1.0".to_string(),
            got: value.to_string(),
        });
    }
    Ok(())
}

fn check_positive(setting: &'static str, value: Duration) -> Result<()> {
    if value.is_zero() {
        return Err(Error::InvalidValue {
            setting,
            expected: "greater than zero".to_string(),
            got: format!("{value:?}"),
        });
    }
    Ok(())
}

impl Config {
    /// Check every rule at once. A caller that changes one setting
    /// should use the matching `set_*` so a rejected value leaves the
    /// previous one in place.
    pub fn validate(&self) -> Result<()> {
        self.fixed.wake_format.validate_for(Target::Wake)?;
        self.fixed.speech_format.validate_for(Target::Speech)?;
        self.fixed.read_format.validate_for(Target::Read)?;

        check_unit("wake threshold", self.tunable.wake_threshold)?;
        check_unit("speech threshold", self.tunable.speech_threshold)?;
        check_positive("silence duration", self.tunable.silence_duration)?;
        check_positive("no speech timeout", self.tunable.no_speech_timeout)?;
        check_positive("ring capacity", self.fixed.ring_capacity)?;

        if self.tunable.max_recording <= self.tunable.silence_duration {
            return Err(Error::InvalidValue {
                setting: "max recording",
                expected: format!(
                    "longer than the silence duration ({:?})",
                    self.tunable.silence_duration
                ),
                got: format!("{:?}", self.tunable.max_recording),
            });
        }

        // Asking for more pre-roll than the history can ever hold is a
        // setting error. Asking for pre-roll the history has not filled
        // yet is not, and yields what exists.
        if self.tunable.pre_roll > self.fixed.ring_capacity {
            return Err(Error::InvalidValue {
                setting: "pre-roll",
                expected: format!(
                    "not longer than the ring capacity ({:?})",
                    self.fixed.ring_capacity
                ),
                got: format!("{:?}", self.tunable.pre_roll),
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default()
            .validate()
            .expect("defaults must validate");
    }

    #[test]
    fn wake_accepts_only_its_model_format() {
        assert!(AudioFormat::mono_16k().validate_for(Target::Wake).is_ok());

        for bad in [
            AudioFormat::new(8_000, 1, SampleType::I16),
            AudioFormat::new(16_000, 2, SampleType::I16),
            AudioFormat::new(16_000, 1, SampleType::F32),
        ] {
            let err = bad.validate_for(Target::Wake).unwrap_err();
            assert!(matches!(err, Error::UnsupportedFormat { .. }), "{err}");
        }
    }

    #[test]
    fn speech_accepts_both_rates_its_model_supports() {
        for rate in [16_000, 8_000] {
            let fmt = AudioFormat::new(rate, 1, SampleType::I16);
            assert!(fmt.validate_for(Target::Speech).is_ok(), "rate {rate}");
        }
        let err = AudioFormat::new(44_100, 1, SampleType::I16)
            .validate_for(Target::Speech)
            .unwrap_err();
        assert!(matches!(err, Error::UnsupportedFormat { .. }));
    }

    #[test]
    fn read_is_free_within_reason() {
        assert!(
            AudioFormat::new(44_100, 2, SampleType::F32)
                .validate_for(Target::Read)
                .is_ok()
        );
        assert!(
            AudioFormat::new(4_000, 1, SampleType::I16)
                .validate_for(Target::Read)
                .is_err()
        );
        assert!(
            AudioFormat::new(16_000, 3, SampleType::I16)
                .validate_for(Target::Read)
                .is_err()
        );
    }

    #[test]
    fn error_names_the_field_and_what_was_expected() {
        let err = AudioFormat::new(48_000, 1, SampleType::I16)
            .validate_for(Target::Wake)
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("sample rate"), "{text}");
        assert!(text.contains("48000"), "{text}");
        assert!(text.contains("16000"), "{text}");
    }

    #[test]
    fn frame_sizes_match_what_each_model_needs() {
        let f16 = AudioFormat::mono_16k();
        assert_eq!(f16.frame_samples(Target::Wake), Some(1280));
        assert_eq!(f16.frame_samples(Target::Speech), Some(512));
        assert_eq!(f16.frame_samples(Target::Read), None);

        let f8 = AudioFormat::new(8_000, 1, SampleType::I16);
        assert_eq!(f8.frame_samples(Target::Speech), Some(256));
    }

    #[test]
    fn thresholds_outside_the_range_are_rejected() {
        let mut cfg = Config::default();
        cfg.tunable.wake_threshold = 1.5;
        assert!(matches!(
            cfg.validate().unwrap_err(),
            Error::InvalidValue { .. }
        ));
    }

    #[test]
    fn max_recording_must_outlast_the_silence_it_waits_for() {
        let mut cfg = Config::default();
        cfg.tunable.max_recording = Duration::from_secs(1);
        assert!(matches!(
            cfg.validate().unwrap_err(),
            Error::InvalidValue { .. }
        ));
    }

    #[test]
    fn pre_roll_beyond_the_ring_is_a_setting_error() {
        let mut cfg = Config::default();
        cfg.tunable.pre_roll = Duration::from_secs(5);
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("pre-roll"), "{err}");
    }
}
