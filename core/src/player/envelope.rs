//! Softening the edges of a sound. Starting or stopping at full level
//! snaps the speaker cone and is heard as a pop; a short ramp each end
//! and a little silence either side removes it.

/// Ramp length at each end. Long enough to hide the step, short enough
/// that a brief alert does not sound faded.
pub const RAMP_MS: u32 = 50;
/// Silence added at each end, so the device has settled before the
/// sound starts and after it ends.
pub const PAD_MS: u32 = 100;

/// Apply the ramp in place. Interleaved samples, so the ramp is applied
/// per frame and every channel of a frame gets the same level.
pub fn apply_ramp(samples: &mut [i16], sample_rate: u32, channels: u16) {
    let channels = channels.max(1) as usize;
    let frames = samples.len() / channels;
    if frames == 0 {
        return;
    }

    let ramp_frames = ((RAMP_MS as f64 / 1000.0) * sample_rate as f64) as usize;
    let ramp_frames = ramp_frames.min(frames / 2);
    if ramp_frames == 0 {
        return;
    }

    for f in 0..ramp_frames {
        let gain = f as f32 / ramp_frames as f32;
        scale_frame(samples, f, channels, gain);

        let tail = frames - 1 - f;
        scale_frame(samples, tail, channels, gain);
    }
}

fn scale_frame(samples: &mut [i16], frame: usize, channels: usize, gain: f32) {
    let start = frame * channels;
    for s in samples[start..start + channels].iter_mut() {
        *s = (*s as f32 * gain) as i16;
    }
}

/// Silence to place at each end.
pub fn padding(sample_rate: u32, channels: u16) -> Vec<i16> {
    let frames = ((PAD_MS as f64 / 1000.0) * sample_rate as f64) as usize;
    vec![0; frames * channels.max(1) as usize]
}

/// Ramp the ends and wrap the result in silence.
pub fn shape(mut samples: Vec<i16>, sample_rate: u32, channels: u16) -> Vec<i16> {
    apply_ramp(&mut samples, sample_rate, channels);

    let pad = padding(sample_rate, channels);
    let mut out = Vec::with_capacity(pad.len() * 2 + samples.len());
    out.extend_from_slice(&pad);
    out.append(&mut samples);
    out.extend_from_slice(&pad);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ends_are_quieter_than_the_middle() {
        let mut samples = vec![10_000i16; 16_000]; // one second at 16 kHz
        apply_ramp(&mut samples, 16_000, 1);

        assert_eq!(samples[0], 0, "the very first sample starts at nothing");
        assert!(samples[100] < 10_000, "still ramping up");
        assert_eq!(samples[8_000], 10_000, "the middle is untouched");
        assert!(
            samples[samples.len() - 100] < 10_000,
            "ramping back down at the end"
        );
        assert_eq!(samples[samples.len() - 1], 0, "and ends at nothing");
    }

    #[test]
    fn the_ramp_never_grows_past_half_the_sound() {
        // Shorter than the full ramp, so the two ends would otherwise
        // overlap and fight over the same samples.
        let mut samples = vec![10_000i16; 4];
        apply_ramp(&mut samples, 16_000, 1);

        assert_eq!(samples[0], 0, "still starts at nothing");
        assert_eq!(samples[3], 0, "still ends at nothing");
        assert_eq!(samples[1], samples[2], "the two ends meet in the middle");
        assert!(samples[1] > 0, "the middle is not wiped out");
    }

    #[test]
    fn a_sound_of_one_frame_is_left_alone() {
        let mut samples = vec![10_000i16; 1];
        apply_ramp(&mut samples, 16_000, 1);
        assert_eq!(samples, vec![10_000i16; 1], "there is no room to ramp");
    }

    #[test]
    fn both_channels_of_a_frame_get_the_same_level() {
        let mut samples = vec![10_000i16; 3_200]; // 1600 stereo frames
        apply_ramp(&mut samples, 16_000, 2);
        for frame in 0..50 {
            assert_eq!(
                samples[frame * 2],
                samples[frame * 2 + 1],
                "frame {frame} came out lopsided"
            );
        }
    }

    #[test]
    fn silence_is_added_at_both_ends() {
        let shaped = shape(vec![10_000i16; 16_000], 16_000, 1);
        let pad = (0.1 * 16_000.0) as usize;

        assert_eq!(shaped.len(), 16_000 + pad * 2);
        assert!(shaped[..pad].iter().all(|s| *s == 0), "leading silence");
        assert!(
            shaped[shaped.len() - pad..].iter().all(|s| *s == 0),
            "trailing silence"
        );
    }

    #[test]
    fn an_empty_sound_does_not_panic() {
        let mut samples: Vec<i16> = Vec::new();
        apply_ramp(&mut samples, 16_000, 1);
        assert!(samples.is_empty());
    }
}
