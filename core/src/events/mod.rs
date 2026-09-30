pub(crate) mod dispatch;

use std::time::Duration;

use crate::config::Device;

/// Why a recording stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The speaker went quiet for long enough.
    Silence,
    /// The length cap was reached while they were still talking.
    MaxLength,
    /// Nothing was ever said.
    NoSpeech,
    /// The application ended it.
    Stopped,
}

impl EndReason {
    /// The name a binding hands an application, as against the
    /// sentence [`Display`](std::fmt::Display) writes for a person.
    /// Held to a word an application can compare or spell out, and
    /// kept in step with the C enum, so that C and Python name the
    /// same four endings the same way.
    pub fn name(&self) -> &'static str {
        match self {
            EndReason::Silence => "silence",
            EndReason::MaxLength => "max_length",
            EndReason::NoSpeech => "no_speech",
            EndReason::Stopped => "stopped",
        }
    }
}

impl std::fmt::Display for EndReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EndReason::Silence => write!(f, "silence"),
            EndReason::MaxLength => write!(f, "maximum length"),
            EndReason::NoSpeech => write!(f, "no speech"),
            EndReason::Stopped => write!(f, "stopped by the application"),
        }
    }
}

/// Something worth telling the application about. Always delivered
/// from the dispatcher thread, never from the one that captures or
/// analyses audio.
#[derive(Debug, Clone)]
pub enum Event {
    /// One of the wake words was heard. `word` is the name it was
    /// added under, and `score` how sure the detector was of it.
    WakeDetected { word: String, score: f32 },
    SpeechEnded {
        audio: Vec<i16>,
        sample_rate: u32,
        reason: EndReason,
        duration: Duration,
    },
    /// A sound reached its own end and its last sample has been heard
    /// from the speaker. One that was stopped is not reported.
    SoundFinished { id: String },
    /// A device, or the work behind it, failed. One that repeats is
    /// reported once rather than for every block of audio.
    DeviceError { device: Device, message: String },
    /// Notifications produced faster than they were consumed. Dropping
    /// the oldest keeps memory flat; this says how many went.
    EventsDropped { count: u64 },
}

impl Event {
    /// Short name, for logs and test failures.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::WakeDetected { .. } => "wake detected",
            Event::SpeechEnded { .. } => "speech ended",
            Event::SoundFinished { .. } => "sound finished",
            Event::DeviceError { .. } => "device error",
            Event::EventsDropped { .. } => "events dropped",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [EndReason; 4] = [
        EndReason::Silence,
        EndReason::MaxLength,
        EndReason::NoSpeech,
        EndReason::Stopped,
    ];

    /// A binding hands these to an application, which compares them.
    /// A space in one means every caller writes it out in full and a
    /// typo goes unnoticed, so the names are held to one word.
    #[test]
    fn every_ending_has_its_own_name_in_one_word() {
        let mut seen = Vec::new();
        for reason in ALL {
            let name = reason.name();
            assert!(!name.is_empty(), "{reason:?} has no name");
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{name:?} is not a name an application can type"
            );
            assert!(!seen.contains(&name), "{name:?} names two endings");
            seen.push(name);
        }
    }

    /// The sentence is for a person reading a log; the name is for an
    /// application. Keeping the two apart is the point, so this fails
    /// if one is ever quietly made the other.
    #[test]
    fn the_name_is_not_the_sentence() {
        assert_eq!(EndReason::MaxLength.name(), "max_length");
        assert_eq!(EndReason::MaxLength.to_string(), "maximum length");
        assert_eq!(EndReason::Stopped.name(), "stopped");
        assert_eq!(EndReason::Stopped.to_string(), "stopped by the application");
    }
}
