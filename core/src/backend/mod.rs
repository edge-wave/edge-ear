#[cfg(feature = "cpal-backend")]
pub mod cpal_backend;
/// A stand-in for real devices. Public on purpose: anyone testing
/// against this library needs a microphone giving known audio on
/// demand. It touches no hardware and reaches nothing outside.
pub mod fake;
pub mod playout;
#[cfg(all(feature = "tinypipewire-backend", target_os = "linux"))]
pub mod tinypipewire_backend;

use std::sync::Arc;
use std::time::Instant;

use crate::capture::Samples;
use crate::config::{AudioFormat, Device, SampleType};
use crate::echo::EchoReference;
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

/// One shape of audio a device says it will take. A range rather than
/// a single rate, because that is how a device describes itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportedFormat {
    pub channels: u16,
    pub min_sample_rate: u32,
    pub max_sample_rate: u32,
    /// What this library would hand over, or take, at that setting.
    pub sample_type: SampleType,
}

impl SupportedFormat {
    /// Whether a format falls inside this, so a request can be checked
    /// before a device is asked to honour it.
    pub fn covers(&self, format: &AudioFormat) -> bool {
        self.channels == format.channels
            && self.sample_type == format.sample_type
            && (self.min_sample_rate..=self.max_sample_rate).contains(&format.sample_rate)
    }
}

/// What the caller wants from a stream. The backend answers with what
/// it could actually open, which may differ.
#[derive(Debug, Clone)]
pub struct FormatRequest {
    pub device: Option<String>,
    /// What the device should be opened at. `None` leaves the choice
    /// to the device, which is what most callers want.
    pub wanted: Option<AudioFormat>,
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

    /// When the sample at `position`, counted from the stream's first, is heard; `None`
    /// until the device takes it. The default, for a backend that cannot tell, says at once.
    fn heard_at(&self, position: u64) -> Option<Instant> {
        let _ = position;
        Some(Instant::now())
    }

    /// Fade out what the device has not taken yet over a few milliseconds, drop
    /// the rest, and say how many samples went. The default drops nothing.
    fn flush(&mut self) -> u64 {
        0
    }

    /// Copy everything the device takes from here on, silence included, into `reference`, so its
    /// echo can be taken out of the microphone. The default cannot and says so with `false`.
    fn tap(&mut self, reference: Arc<EchoReference>) -> bool {
        let _ = reference;
        false
    }

    fn stop(&mut self) -> Result<()>;
}

/// Where audio comes from and goes to. The default wraps cpal; the
/// fake reads files and drives time itself, so every scenario is
/// testable without hardware.
pub trait AudioBackend: Send {
    fn open_input(&mut self, req: &FormatRequest) -> Result<Box<dyn InputStream>>;
    fn open_output(&mut self, req: &FormatRequest) -> Result<Box<dyn OutputStream>>;
    fn input_devices(&self) -> Result<Vec<DeviceInfo>>;
    fn output_devices(&self) -> Result<Vec<DeviceInfo>>;
    /// What one device will take. `None` asks about the default.
    fn input_formats(&self, device: Option<&str>) -> Result<Vec<SupportedFormat>>;
    fn output_formats(&self, device: Option<&str>) -> Result<Vec<SupportedFormat>>;

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
