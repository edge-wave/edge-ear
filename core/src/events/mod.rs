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
    WakeDetected {
        score: f32,
    },
    SpeechEnded {
        audio: Vec<i16>,
        sample_rate: u32,
        reason: EndReason,
        duration: Duration,
    },
    SoundFinished {
        id: String,
    },
    DeviceError {
        device: Device,
        message: String,
    },
    /// Notifications produced faster than they were consumed. Dropping
    /// the oldest keeps memory flat; this says how many went.
    EventsDropped {
        count: u64,
    },
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
