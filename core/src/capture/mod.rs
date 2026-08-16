pub mod ring;

use std::time::Instant;

use crate::config::{AudioFormat, SampleType};

/// Audio samples in whichever form the consumer asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Samples {
    I16(Vec<i16>),
    F32(Vec<f32>),
}

impl Samples {
    pub fn len(&self) -> usize {
        match self {
            Samples::I16(v) => v.len(),
            Samples::F32(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn sample_type(&self) -> SampleType {
        match self {
            Samples::I16(_) => SampleType::I16,
            Samples::F32(_) => SampleType::F32,
        }
    }

    pub fn as_i16(&self) -> Option<&[i16]> {
        match self {
            Samples::I16(v) => Some(v),
            Samples::F32(_) => None,
        }
    }

    pub fn as_f32(&self) -> Option<&[f32]> {
        match self {
            Samples::F32(v) => Some(v),
            Samples::I16(_) => None,
        }
    }
}

/// One block of live audio, delivered to one consumer.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioChunk {
    pub samples: Samples,
    pub format: AudioFormat,
    /// When the device produced this, not when it was read. A slow
    /// consumer still sees correct timing.
    pub captured_at: Instant,
    /// Chunks this consumer lost before this one. Above zero only after
    /// it fell behind.
    pub dropped_before: u64,
}

impl AudioChunk {
    pub fn new(samples: Samples, format: AudioFormat, captured_at: Instant) -> Self {
        Self {
            samples,
            format,
            captured_at,
            dropped_before: 0,
        }
    }

    /// How long this chunk covers.
    pub fn duration(&self) -> std::time::Duration {
        let frames = self.samples.len() / self.format.channels.max(1) as usize;
        std::time::Duration::from_secs_f64(frames as f64 / self.format.sample_rate as f64)
    }
}

/// The three independent readers of live audio. They share nothing but
/// the chunks handed to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConsumerKind {
    Wake,
    Speech,
    Read,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_follows_the_format() {
        let chunk = AudioChunk::new(
            Samples::I16(vec![0; 1600]),
            AudioFormat::mono_16k(),
            Instant::now(),
        );
        assert_eq!(chunk.duration(), std::time::Duration::from_millis(100));
    }

    #[test]
    fn stereo_duration_counts_frames_not_samples() {
        let format = AudioFormat::new(16_000, 2, SampleType::I16);
        let chunk = AudioChunk::new(Samples::I16(vec![0; 3200]), format, Instant::now());
        assert_eq!(chunk.duration(), std::time::Duration::from_millis(100));
    }
}
