#[cfg(feature = "cpal-backend")]
pub mod cpal_backend;
/// A stand-in for real devices, for tests.
///
/// Public on purpose: anyone writing tests against this library needs a
/// microphone that produces known audio on demand. It touches no
/// hardware and reaches nothing outside the process.
pub mod fake;

use crate::capture::Samples;
use crate::config::{AudioFormat, Device};
use crate::error::Result;

/// What a device says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The one that picks this device again later. Unique, stable, and
    /// safe to store. Names are not: a machine can show several
    /// devices with exactly the same name.
    pub id: String,
    /// For showing to a person. May repeat across devices.
    pub name: String,
    pub is_default: bool,
}

/// What the caller wants from a stream. The backend answers with what
/// it could actually open, which may differ.
#[derive(Debug, Clone)]
pub struct FormatRequest {
    pub device: Option<String>,
    pub preferred: AudioFormat,
}

/// A microphone stream. Owned by exactly one capture thread.
pub trait InputStream: Send {
    /// The format the device actually produces. Conversion to what each
    /// consumer asked for happens above this.
    fn format(&self) -> AudioFormat;

    /// Block until the next block of audio is available.
    fn read(&mut self) -> Result<Samples>;

    fn stop(&mut self) -> Result<()>;
}

/// A speaker stream. Owned by exactly one player.
pub trait OutputStream: Send {
    fn format(&self) -> AudioFormat;

    /// Hand samples to the device. Returns once they are queued.
    fn write(&mut self, samples: &Samples) -> Result<()>;

    fn stop(&mut self) -> Result<()>;
}

/// Where audio comes from and goes to.
///
/// The default implementation wraps cpal. The fake one reads files and
/// drives time itself, so every scenario is testable without hardware.
pub trait AudioBackend: Send {
    fn open_input(&mut self, req: &FormatRequest) -> Result<Box<dyn InputStream>>;
    fn open_output(&mut self, req: &FormatRequest) -> Result<Box<dyn OutputStream>>;
    fn input_devices(&self) -> Result<Vec<DeviceInfo>>;
    fn output_devices(&self) -> Result<Vec<DeviceInfo>>;

    /// Name for error messages and logs.
    fn describe(&self) -> &'static str;
}

/// Which device a failure came from. Kept next to the trait so backends
/// report consistently.
pub fn device_of(is_input: bool) -> Device {
    if is_input {
        Device::Input
    } else {
        Device::Output
    }
}
