use std::path::PathBuf;

use thiserror::Error;

use crate::config::{Device, Target};

pub type Result<T> = std::result::Result<T, Error>;

/// Every way a call can fail. Each wrong-order call has its own
/// variant so an application can tell them apart.
#[derive(Debug, Error)]
pub enum Error {
    #[error("capture is not running")]
    NotRunning,

    #[error("capture is already running")]
    AlreadyRunning,

    #[error("{what} can only be set before capture starts; stop capture first")]
    RunningNotAllowed { what: &'static str },

    #[error(
        "{what} cannot be changed while a recording is open; it would not affect the recording already running"
    )]
    RecordingOpen { what: &'static str },

    #[error("this handle has been destroyed")]
    Destroyed,

    #[error("no wake word model has been loaded")]
    NoWakeModel,

    #[error("model file not found: {path}")]
    ModelNotFound { path: PathBuf },

    #[error("model file could not be read: {path}: {reason}")]
    ModelUnreadable { path: PathBuf, reason: String },

    #[error("not a usable model: {path}: {reason}")]
    ModelInvalid { path: PathBuf, reason: String },

    #[error("{target} does not accept {field} {got}; it requires {expected}")]
    UnsupportedFormat {
        target: Target,
        field: &'static str,
        got: String,
        expected: String,
    },

    #[error("the {device} device does not open at {got}; it offers {offered}")]
    DeviceFormat {
        device: Device,
        got: String,
        offered: String,
    },

    #[error("{setting} must be {expected}, got {got}")]
    InvalidValue {
        setting: &'static str,
        expected: String,
        got: String,
    },

    #[error("no {0} device is available")]
    NoDevice(Device),

    #[error("the system refused access to the microphone")]
    PermissionDenied,

    #[error("the {0} device was lost")]
    DeviceLost(Device),

    #[error("no sound is registered with id {0}")]
    UnknownSound(String),

    #[error("timed out waiting for audio")]
    Timeout,

    #[error("capture stopped while waiting")]
    Stopped,

    #[error("{device} device failed: {reason}")]
    Backend { device: Device, reason: String },

    #[error("audio conversion failed: {reason}")]
    Conversion { reason: String },
}

impl Error {
    /// True when the call was made in a state where it could not succeed.
    /// Used by tests that check every entry point rejects wrong ordering.
    pub fn is_wrong_state(&self) -> bool {
        matches!(
            self,
            Error::NotRunning
                | Error::AlreadyRunning
                | Error::RunningNotAllowed { .. }
                | Error::RecordingOpen { .. }
                | Error::Destroyed
                | Error::NoWakeModel
        )
    }
}
