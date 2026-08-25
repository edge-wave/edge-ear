//! Handing a notification to C for the length of one call.

use std::ffi::{CString, c_char, c_void};

use edge_ear_core::events::{EndReason, Event};

#[repr(i32)]
#[derive(Clone, Copy)]
/// Which notification arrived.
#[allow(non_camel_case_types)]
pub enum edge_ear_event_kind {
    /// The wake word was heard.
    EDGE_EAR_EVENT_WAKE_DETECTED = 1,
    /// A recording ended and the audio is here.
    EDGE_EAR_EVENT_SPEECH_ENDED,
    /// A sound reached its own end. One that was stopped does not
    /// arrive here.
    EDGE_EAR_EVENT_SOUND_FINISHED,
    /// A device failed. Capture has stopped.
    EDGE_EAR_EVENT_DEVICE_ERROR,
    /// Notifications were thrown away because a handler could not
    /// keep up.
    EDGE_EAR_EVENT_EVENTS_DROPPED,
}

#[repr(i32)]
#[derive(Clone, Copy)]
/// What brought a recording to an end.
#[allow(non_camel_case_types)]
pub enum edge_ear_end_reason {
    /// No recording ended; this notification is something else.
    EDGE_EAR_END_NONE = 0,
    /// The speaker went quiet for the silence duration.
    EDGE_EAR_END_SILENCE,
    /// The length cap was reached while someone was still talking.
    EDGE_EAR_END_MAX_LENGTH,
    /// Nobody spoke at all before the timeout.
    EDGE_EAR_END_NO_SPEECH,
    /// The application ended it.
    EDGE_EAR_END_STOPPED,
}

impl From<EndReason> for edge_ear_end_reason {
    fn from(reason: EndReason) -> Self {
        match reason {
            EndReason::Silence => edge_ear_end_reason::EDGE_EAR_END_SILENCE,
            EndReason::MaxLength => edge_ear_end_reason::EDGE_EAR_END_MAX_LENGTH,
            EndReason::NoSpeech => edge_ear_end_reason::EDGE_EAR_END_NO_SPEECH,
            EndReason::Stopped => edge_ear_end_reason::EDGE_EAR_END_STOPPED,
        }
    }
}

/// One notification, as C sees it. Every pointer borrows from the call
/// that raised it and stops being valid when the handler returns.
#[repr(C)]
#[allow(non_camel_case_types)]
pub struct edge_ear_event {
    /// Which notification this is. It decides which fields below mean
    /// anything; the rest are zero or null.
    pub kind: edge_ear_event_kind,
    /// Wake detected: how sure the detector was, from 0.0 to 1.0.
    pub score: f32,
    /// Speech ended: the recording, including any pre-roll.
    pub audio: *const i16,
    /// Speech ended: how many samples `audio` holds.
    pub audio_len: usize,
    /// Speech ended: the rate those samples were taken at.
    pub sample_rate: u32,
    /// Speech ended: what brought the recording to an end.
    pub reason: edge_ear_end_reason,
    /// Speech ended: audio counted after the recording opened, which
    /// leaves out any pre-roll in front of it.
    pub duration_secs: f64,
    /// Sound finished: the name it was registered under.
    pub sound_id: *const c_char,
    /// Device error: what went wrong, in plain words.
    pub message: *const c_char,
    /// Events dropped: how many the application was never told about,
    /// because a handler could not keep up.
    pub dropped: u64,
}

/// Called for every notification. The event stops being valid when it
/// returns, so anything kept must be copied first.
#[allow(non_camel_case_types)]
pub type edge_ear_event_cb =
    Option<unsafe extern "C" fn(event: *const edge_ear_event, user: *mut c_void)>;

/// A callback and the pointer it came with, moved onto the dispatcher
/// thread together. The raw pointer is why this is not `Send` on its
/// own: the caller owns it and keeps it alive until the handle goes.
pub struct Registered {
    pub callback: edge_ear_event_cb,
    pub user: *mut c_void,
}

unsafe impl Send for Registered {}
unsafe impl Sync for Registered {}

impl Registered {
    /// Lay one event out for C and call the handler with it.
    pub fn deliver(&self, event: Event) {
        let Some(callback) = self.callback else {
            return;
        };

        // These own the strings for the length of the call and are
        // dropped after it, which is exactly the promised lifetime.
        let mut sound_id = None;
        let mut message = None;

        let laid_out = match &event {
            Event::WakeDetected { score } => edge_ear_event {
                kind: edge_ear_event_kind::EDGE_EAR_EVENT_WAKE_DETECTED,
                score: *score,
                ..edge_ear_event::empty()
            },
            Event::SpeechEnded {
                audio,
                sample_rate,
                reason,
                duration,
            } => edge_ear_event {
                kind: edge_ear_event_kind::EDGE_EAR_EVENT_SPEECH_ENDED,
                audio: audio.as_ptr(),
                audio_len: audio.len(),
                sample_rate: *sample_rate,
                reason: (*reason).into(),
                duration_secs: duration.as_secs_f64(),
                ..edge_ear_event::empty()
            },
            Event::SoundFinished { id } => {
                let text = CString::new(id.as_str()).unwrap_or_default();
                let ptr = text.as_ptr();
                sound_id = Some(text);
                edge_ear_event {
                    kind: edge_ear_event_kind::EDGE_EAR_EVENT_SOUND_FINISHED,
                    sound_id: ptr,
                    ..edge_ear_event::empty()
                }
            }
            Event::DeviceError { message: text, .. } => {
                let owned = CString::new(text.as_str()).unwrap_or_default();
                let ptr = owned.as_ptr();
                message = Some(owned);
                edge_ear_event {
                    kind: edge_ear_event_kind::EDGE_EAR_EVENT_DEVICE_ERROR,
                    message: ptr,
                    ..edge_ear_event::empty()
                }
            }
            Event::EventsDropped { count } => edge_ear_event {
                kind: edge_ear_event_kind::EDGE_EAR_EVENT_EVENTS_DROPPED,
                dropped: *count,
                ..edge_ear_event::empty()
            },
        };

        // Safe as far as this side goes: the pointer is valid for the
        // whole call and nothing here is freed until it returns.
        unsafe { callback(&laid_out, self.user) };
        drop(sound_id);
        drop(message);
    }
}

impl edge_ear_event {
    fn empty() -> Self {
        Self {
            kind: edge_ear_event_kind::EDGE_EAR_EVENT_WAKE_DETECTED,
            score: 0.0,
            audio: std::ptr::null(),
            audio_len: 0,
            sample_rate: 0,
            reason: edge_ear_end_reason::EDGE_EAR_END_NONE,
            duration_secs: 0.0,
            sound_id: std::ptr::null(),
            message: std::ptr::null(),
            dropped: 0,
        }
    }
}
