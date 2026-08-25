//! Turning core errors into the codes a C caller sees.

use std::cell::RefCell;
use std::ffi::{CString, c_char};

use edge_ear_core::error::Error;

/// What went wrong. Zero is success; everything else is negative, one
/// value for each failure the core reports.
#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(non_camel_case_types)]
pub enum edge_ear_error {
    /// It worked.
    EDGE_EAR_OK = 0,
    /// Capture is not running.
    EDGE_EAR_NOT_RUNNING = -1,
    /// Capture is already running.
    EDGE_EAR_ALREADY_RUNNING = -2,
    /// Can only be set before capture starts.
    EDGE_EAR_RUNNING_NOT_ALLOWED = -3,
    /// Refused while a recording is open, because a recording
    /// follows the rules it opened with.
    EDGE_EAR_RECORDING_OPEN = -4,
    /// The handle has been freed.
    EDGE_EAR_DESTROYED = -5,
    /// No wake word model has been loaded.
    EDGE_EAR_NO_WAKE_MODEL = -6,
    /// No model file at that path.
    EDGE_EAR_MODEL_NOT_FOUND = -7,
    /// The model file could not be read.
    EDGE_EAR_MODEL_UNREADABLE = -8,
    /// The file is not a model this pipeline can use.
    EDGE_EAR_MODEL_INVALID = -9,
    /// That part of the library does not take this audio format.
    EDGE_EAR_UNSUPPORTED_FORMAT = -10,
    /// The value is outside what the setting allows.
    EDGE_EAR_INVALID_VALUE = -11,
    /// No such device, or none available at all.
    EDGE_EAR_NO_DEVICE = -12,
    /// The system refused access to the microphone.
    EDGE_EAR_PERMISSION_DENIED = -13,
    /// The device went away while it was in use.
    EDGE_EAR_DEVICE_LOST = -14,
    /// No sound is registered under that name.
    EDGE_EAR_UNKNOWN_SOUND = -15,
    /// Nothing arrived before the time given ran out.
    EDGE_EAR_TIMEOUT = -16,
    /// Capture stopped while this call was waiting.
    EDGE_EAR_STOPPED = -17,
    /// The device itself failed. The message says how.
    EDGE_EAR_BACKEND = -18,
    /// The audio could not be converted to the wanted format.
    EDGE_EAR_CONVERSION = -19,
    /// Something required was null.
    EDGE_EAR_NULL_ARGUMENT = -20,
    /// A string was not valid UTF-8. It is refused rather than
    /// replaced or cut short.
    EDGE_EAR_NOT_UTF8 = -21,
}

pub use edge_ear_error::*;

/// Success, as the plain number every call returns.
pub(crate) const OK: i32 = edge_ear_error::EDGE_EAR_OK as i32;

pub fn code_of(error: &Error) -> edge_ear_error {
    match error {
        Error::NotRunning => EDGE_EAR_NOT_RUNNING,
        Error::AlreadyRunning => EDGE_EAR_ALREADY_RUNNING,
        Error::RunningNotAllowed { .. } => EDGE_EAR_RUNNING_NOT_ALLOWED,
        Error::RecordingOpen { .. } => EDGE_EAR_RECORDING_OPEN,
        Error::Destroyed => EDGE_EAR_DESTROYED,
        Error::NoWakeModel => EDGE_EAR_NO_WAKE_MODEL,
        Error::ModelNotFound { .. } => EDGE_EAR_MODEL_NOT_FOUND,
        Error::ModelUnreadable { .. } => EDGE_EAR_MODEL_UNREADABLE,
        Error::ModelInvalid { .. } => EDGE_EAR_MODEL_INVALID,
        Error::UnsupportedFormat { .. } => EDGE_EAR_UNSUPPORTED_FORMAT,
        Error::InvalidValue { .. } => EDGE_EAR_INVALID_VALUE,
        Error::NoDevice(_) => EDGE_EAR_NO_DEVICE,
        Error::PermissionDenied => EDGE_EAR_PERMISSION_DENIED,
        Error::DeviceLost(_) => EDGE_EAR_DEVICE_LOST,
        Error::UnknownSound(_) => EDGE_EAR_UNKNOWN_SOUND,
        Error::Timeout => EDGE_EAR_TIMEOUT,
        Error::Stopped => EDGE_EAR_STOPPED,
        Error::Backend { .. } => EDGE_EAR_BACKEND,
        Error::Conversion { .. } => EDGE_EAR_CONVERSION,
    }
}

thread_local! {
    /// The message behind the last failing call on this thread. Kept
    /// alive here so the pointer handed out stays valid until the next.
    static LAST: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Record a failure and give back its code.
pub fn fail(error: &Error) -> i32 {
    remember(error.to_string());
    code_of(error) as i32
}

/// Record a failure this crate raised itself, one the core has no
/// variant for.
pub fn fail_with(code: edge_ear_error, message: &str) -> i32 {
    remember(message.to_string());
    code as i32
}

fn remember(message: String) {
    let text =
        CString::new(message).unwrap_or_else(|_| c"the message contained a null byte".into());
    LAST.with(|slot| *slot.borrow_mut() = Some(text));
}

/// The message behind the last failing call on this thread, or an empty
/// string when nothing has failed yet.
pub fn last_message() -> *const c_char {
    LAST.with(|slot| match slot.borrow().as_ref() {
        Some(text) => text.as_ptr(),
        None => c"".as_ptr(),
    })
}

/// Turn a `Result<()>` into a code, remembering any message.
pub fn report(result: edge_ear_core::error::Result<()>) -> i32 {
    match result {
        Ok(()) => OK,
        Err(e) => fail(&e),
    }
}
