//! Python binding for edge-ear. Binds the core crate directly, adding
//! no behaviour beyond handing what core logs to Python's `logging`.
//! The interpreter lock is held only where Python itself is touched:
//! every call that waits on a device or a thread lets go of it.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

use edge_ear_core::config::{AudioFormat, SampleType, Target};
use edge_ear_core::error::Error;
use edge_ear_core::events::{EndReason, Event};
use edge_ear_core::{EdgeEar as Core, Samples, SoundSource};

// One exception per way a call can fail, so an application can catch a
// refused microphone separately from a missing one.
pyo3::create_exception!(edge_ear, EdgeEarError, PyException);
pyo3::create_exception!(edge_ear, NotRunning, EdgeEarError);
pyo3::create_exception!(edge_ear, AlreadyRunning, EdgeEarError);
pyo3::create_exception!(edge_ear, RunningNotAllowed, EdgeEarError);
pyo3::create_exception!(edge_ear, RecordingOpen, EdgeEarError);
pyo3::create_exception!(edge_ear, Destroyed, EdgeEarError);
pyo3::create_exception!(edge_ear, NoWakeModel, EdgeEarError);
pyo3::create_exception!(edge_ear, ModelNotFound, EdgeEarError);
pyo3::create_exception!(edge_ear, ModelUnreadable, EdgeEarError);
pyo3::create_exception!(edge_ear, ModelInvalid, EdgeEarError);
pyo3::create_exception!(edge_ear, UnsupportedFormat, EdgeEarError);
pyo3::create_exception!(edge_ear, InvalidValue, EdgeEarError);
pyo3::create_exception!(edge_ear, NoDevice, EdgeEarError);
pyo3::create_exception!(edge_ear, PermissionDenied, EdgeEarError);
pyo3::create_exception!(edge_ear, DeviceLost, EdgeEarError);
pyo3::create_exception!(edge_ear, UnknownSound, EdgeEarError);
pyo3::create_exception!(edge_ear, UnknownWakeWord, EdgeEarError);
pyo3::create_exception!(edge_ear, Timeout, EdgeEarError);
pyo3::create_exception!(edge_ear, Stopped, EdgeEarError);
pyo3::create_exception!(edge_ear, BackendError, EdgeEarError);
pyo3::create_exception!(edge_ear, ConversionError, EdgeEarError);

/// Held so the cached levels can be dropped when Python's own change.
static LOG_RESET: OnceLock<pyo3_log::ResetHandle> = OnceLock::new();

/// Hand what core logs to Python's `logging`, under the `edge_ear`
/// logger. Levels are cached on this side so the audio threads stay
/// off the interpreter lock; `reset_logging` clears that cache.
fn install_logging(py: Python<'_>) -> PyResult<()> {
    let logger =
        pyo3_log::Logger::new(py, pyo3_log::Caching::LoggersAndLevels)?.set_prefix("edge_ear");
    // An application that installed its own logger keeps it, and core
    // then logs wherever that one sends it.
    if let Ok(handle) = logger.install() {
        let _ = LOG_RESET.set(handle);
    }
    Ok(())
}

/// Look at the Python logging levels again. Needed only when they are
/// changed after this module has already logged something.
#[pyfunction]
fn reset_logging() {
    if let Some(handle) = LOG_RESET.get() {
        handle.reset();
    }
}

fn to_py(error: Error) -> PyErr {
    let text = error.to_string();
    match error {
        Error::NotRunning => NotRunning::new_err(text),
        Error::AlreadyRunning => AlreadyRunning::new_err(text),
        Error::RunningNotAllowed { .. } => RunningNotAllowed::new_err(text),
        Error::RecordingOpen { .. } => RecordingOpen::new_err(text),
        Error::Destroyed => Destroyed::new_err(text),
        Error::NoWakeModel => NoWakeModel::new_err(text),
        Error::ModelNotFound { .. } => ModelNotFound::new_err(text),
        Error::ModelUnreadable { .. } => ModelUnreadable::new_err(text),
        Error::ModelInvalid { .. } => ModelInvalid::new_err(text),
        Error::UnsupportedFormat { .. } | Error::DeviceFormat { .. } => {
            UnsupportedFormat::new_err(text)
        }
        Error::InvalidValue { .. } => InvalidValue::new_err(text),
        Error::NoDevice(_) => NoDevice::new_err(text),
        Error::PermissionDenied => PermissionDenied::new_err(text),
        Error::DeviceLost(_) => DeviceLost::new_err(text),
        Error::UnknownSound(_) => UnknownSound::new_err(text),
        Error::UnknownWakeWord(_) => UnknownWakeWord::new_err(text),
        Error::Timeout => Timeout::new_err(text),
        Error::Stopped => Stopped::new_err(text),
        Error::Backend { .. } => BackendError::new_err(text),
        Error::Conversion { .. } => ConversionError::new_err(text),
    }
}

/// A device the library can open.
#[pyclass(frozen, get_all, skip_from_py_object)]
#[derive(Clone)]
pub struct Device {
    /// Picks out this one device. Store this, not the name.
    pub id: String,
    /// For showing to a person. Names repeat across devices.
    pub name: String,
    pub is_default: bool,
}

#[pymethods]
impl Device {
    fn __repr__(&self) -> String {
        let mark = if self.is_default { ", default" } else { "" };
        format!("Device({}, {:?}{mark})", self.id, self.name)
    }
}

/// One shape of audio a device says it will take.
#[pyclass(frozen, get_all, skip_from_py_object)]
#[derive(Clone)]
pub struct SupportedFormat {
    pub channels: u16,
    /// The lowest rate at this setting, and the highest. Equal when a
    /// device offers one rate rather than a span.
    pub min_sample_rate: u32,
    pub max_sample_rate: u32,
    /// "i16" or "f32".
    pub sample_type: String,
}

#[pymethods]
impl SupportedFormat {
    fn __repr__(&self) -> String {
        let rate = if self.min_sample_rate == self.max_sample_rate {
            format!("{} Hz", self.min_sample_rate)
        } else {
            format!("{}-{} Hz", self.min_sample_rate, self.max_sample_rate)
        };
        format!(
            "SupportedFormat({}ch, {rate}, {})",
            self.channels, self.sample_type
        )
    }
}

/// A block of live audio.
#[pyclass(frozen)]
pub struct AudioChunk {
    samples: Vec<i16>,
    #[pyo3(get)]
    sample_rate: u32,
    #[pyo3(get)]
    channels: u16,
    /// Blocks this reader lost before this one. Above zero only after
    /// it fell behind.
    #[pyo3(get)]
    dropped_before: u64,
}

#[pymethods]
impl AudioChunk {
    /// The audio as raw bytes, 16-bit little endian.
    #[getter]
    fn audio<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &to_bytes(&self.samples))
    }

    /// The audio as a numpy array, when numpy is installed.
    fn numpy<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        as_numpy(py, &self.samples)
    }

    fn __len__(&self) -> usize {
        self.samples.len()
    }

    fn __repr__(&self) -> String {
        format!(
            "AudioChunk({} samples, {} Hz, {} ch)",
            self.samples.len(),
            self.sample_rate,
            self.channels
        )
    }
}

fn to_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Hand the audio over as a numpy array when numpy is there, so an
/// application does not have to unpack the bytes itself.
fn as_numpy<'py>(py: Python<'py>, samples: &[i16]) -> PyResult<Bound<'py, PyAny>> {
    let numpy = py.import("numpy")?;
    let bytes = PyBytes::new(py, &to_bytes(samples));
    let kwargs = PyDict::new(py);
    kwargs.set_item("dtype", numpy.getattr("int16")?)?;
    numpy.call_method("frombuffer", (bytes,), Some(&kwargs))
}

/// Something the library is telling the application about.
#[pyclass(frozen, subclass)]
pub struct AudioEvent;

#[pyclass(frozen, extends = AudioEvent)]
pub struct WakeDetected {
    #[pyo3(get)]
    word: String,
    #[pyo3(get)]
    score: f32,
}

#[pymethods]
impl WakeDetected {
    fn __repr__(&self) -> String {
        format!(
            "WakeDetected(word={:?}, score={:.3})",
            self.word, self.score
        )
    }
}

#[pyclass(frozen, extends = AudioEvent)]
pub struct SpeechEnded {
    samples: Vec<i16>,
    #[pyo3(get)]
    sample_rate: u32,
    /// Why it ended, as one of the names on [`EndReason`].
    #[pyo3(get)]
    reason: String,
    /// Seconds.
    #[pyo3(get)]
    duration: f64,
}

#[pymethods]
impl SpeechEnded {
    #[getter]
    fn audio<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &to_bytes(&self.samples))
    }

    fn numpy<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        as_numpy(py, &self.samples)
    }

    fn __repr__(&self) -> String {
        format!(
            "SpeechEnded({} samples, {:.2}s, {})",
            self.samples.len(),
            self.duration,
            self.reason
        )
    }
}

#[pyclass(frozen, extends = AudioEvent)]
pub struct SoundFinished {
    #[pyo3(get)]
    id: String,
}

#[pymethods]
impl SoundFinished {
    fn __repr__(&self) -> String {
        format!("SoundFinished({:?})", self.id)
    }
}

/// A device, or the work behind it, failed. One that repeats is
/// reported once rather than for every block of audio.
#[pyclass(frozen, extends = AudioEvent)]
pub struct DeviceError {
    /// "input" or "output".
    #[pyo3(get)]
    device: String,
    #[pyo3(get)]
    message: String,
}

#[pymethods]
impl DeviceError {
    fn __repr__(&self) -> String {
        format!("DeviceError({}, {:?})", self.device, self.message)
    }
}

#[pyclass(frozen, extends = AudioEvent)]
pub struct EventsDropped {
    #[pyo3(get)]
    count: u64,
}

#[pymethods]
impl EventsDropped {
    fn __repr__(&self) -> String {
        format!("EventsDropped({})", self.count)
    }
}

/// The four endings a recording can have, as the names
/// `SpeechEnded.reason` carries. They are plain strings, so a
/// comparison against one reads the same either way; naming them here
/// means a misspelling fails at the attribute rather than by quietly
/// never matching. C names the same four through
/// `edge_ear_end_reason`.
#[pyclass(frozen, name = "EndReason")]
pub struct EndReasonNames;

#[pymethods]
impl EndReasonNames {
    /// The speaker went quiet for the silence duration.
    #[classattr]
    const SILENCE: &'static str = "silence";
    /// The length cap was reached while someone was still talking.
    #[classattr]
    const MAX_LENGTH: &'static str = "max_length";
    /// Nobody spoke at all before the timeout.
    #[classattr]
    const NO_SPEECH: &'static str = "no_speech";
    /// The application ended it.
    #[classattr]
    const STOPPED: &'static str = "stopped";
}

fn reason_name(reason: EndReason) -> String {
    reason.name().to_string()
}

/// Build the Python object for one event. Runs on the thread that
/// delivers notifications, which is the only place the interpreter lock
/// is taken.
fn event_to_py(py: Python<'_>, event: Event) -> PyResult<Py<PyAny>> {
    Ok(match event {
        Event::WakeDetected { word, score } => {
            Py::new(py, (WakeDetected { word, score }, AudioEvent))?.into_any()
        }
        Event::SpeechEnded {
            audio,
            sample_rate,
            reason,
            duration,
        } => Py::new(
            py,
            (
                SpeechEnded {
                    samples: audio,
                    sample_rate,
                    reason: reason_name(reason),
                    duration: duration.as_secs_f64(),
                },
                AudioEvent,
            ),
        )?
        .into_any(),
        Event::SoundFinished { id } => Py::new(py, (SoundFinished { id }, AudioEvent))?.into_any(),
        Event::DeviceError { device, message } => Py::new(
            py,
            (
                DeviceError {
                    device: device.to_string(),
                    message,
                },
                AudioEvent,
            ),
        )?
        .into_any(),
        Event::EventsDropped { count } => {
            Py::new(py, (EventsDropped { count }, AudioEvent))?.into_any()
        }
    })
}

fn sample_type(name: &str) -> PyResult<SampleType> {
    match name {
        "i16" | "int16" => Ok(SampleType::I16),
        "f32" | "float32" => Ok(SampleType::F32),
        other => Err(InvalidValue::new_err(format!(
            "sample type must be i16 or f32, got {other:?}"
        ))),
    }
}

fn target(name: &str) -> PyResult<Target> {
    match name {
        "read" => Ok(Target::Read),
        "wake" => Ok(Target::Wake),
        "speech" => Ok(Target::Speech),
        other => Err(InvalidValue::new_err(format!(
            "target must be read, wake, or speech, got {other:?}"
        ))),
    }
}

/// One running instance. Owns one microphone and one speaker.
#[pyclass(frozen)]
pub struct EdgeEar {
    core: Arc<Core>,
}

#[pymethods]
impl EdgeEar {
    #[new]
    fn new() -> PyResult<Self> {
        Ok(Self {
            core: Arc::new(Core::new().map_err(to_py)?),
        })
    }

    // ── lifecycle ────────────────────────────────────────────────────

    // These wait on the worker threads, and a worker that logs needs
    // the interpreter lock, so it is released for the wait.
    fn start(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.core.start()).map_err(to_py)
    }

    fn stop(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.core.stop()).map_err(to_py)
    }

    fn close(&self, py: Python<'_>) {
        py.detach(|| self.core.destroy());
    }

    #[getter]
    fn is_running(&self) -> bool {
        self.core.is_running()
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, py: Python<'_>, _args: &Bound<'_, PyAny>) -> bool {
        // Leaving the block releases the devices, even when the block
        // is leaving because something went wrong.
        py.detach(|| self.core.destroy());
        false
    }

    // ── reading live audio ───────────────────────────────────────────

    /// Take the next block of live audio, waiting for it to arrive.
    /// `timeout` is in seconds; `None` waits until audio comes or
    /// capture stops.
    #[pyo3(signature = (timeout = None))]
    fn read(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<AudioChunk> {
        let wait = timeout.map(Duration::from_secs_f64);
        // Let other Python threads run while this one waits.
        let chunk = py.detach(|| self.core.read(wait)).map_err(to_py)?;

        let samples = match &chunk.samples {
            edge_ear_core::Samples::I16(v) => v.clone(),
            edge_ear_core::Samples::F32(v) => v
                .iter()
                .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                .collect(),
        };
        Ok(AudioChunk {
            samples,
            sample_rate: chunk.format.sample_rate,
            channels: chunk.format.channels,
            dropped_before: chunk.dropped_before,
        })
    }

    // ── notifications ────────────────────────────────────────────────

    /// Set the handler called for every notification. It runs on the
    /// delivering thread, holding the interpreter lock only for the
    /// call, so a slow one delays later notifications and nothing else.
    fn on_event(&self, handler: Py<PyAny>) -> PyResult<()> {
        self.core
            .on_event(move |event| {
                Python::attach(|py| {
                    match event_to_py(py, event) {
                        Ok(object) => {
                            if let Err(e) = handler.call1(py, (object,)) {
                                // A handler that raises must not take the
                                // delivery thread down with it.
                                e.print(py);
                            }
                        }
                        Err(e) => e.print(py),
                    }
                });
            })
            .map_err(to_py)
    }

    // ── wake word ────────────────────────────────────────────────────

    /// Supply the two models every wake word shares. Neither knows any
    /// word, and this library ships neither.
    fn load_wake_features(&self, spectrogram: PathBuf, features: PathBuf) -> PyResult<()> {
        self.core
            .load_wake_features(&spectrogram, &features)
            .map_err(to_py)
    }

    /// Add a phrase to listen for, under the name its detections carry.
    /// Without a name it is called after its file.
    #[pyo3(signature = (path, name = None))]
    fn add_wake_model(&self, path: PathBuf, name: Option<&str>) -> PyResult<()> {
        self.core.add_wake_model(name, &path).map_err(to_py)
    }

    /// Stop listening for one phrase and forget its model.
    fn remove_wake_model(&self, name: &str) -> PyResult<()> {
        self.core.remove_wake_model(name).map_err(to_py)
    }

    /// The names of the words listened for, in the order they were added.
    #[getter]
    fn wake_models(&self) -> Vec<String> {
        self.core.wake_models()
    }

    /// Start listening for the wake word. Naming a sound plays it on
    /// detection and holds off counting silence until it ends.
    #[pyo3(signature = (alert = None))]
    fn enable_wake(&self, alert: Option<&str>) -> PyResult<()> {
        self.core.enable_wake(alert).map_err(to_py)
    }

    fn disable_wake(&self) -> PyResult<()> {
        self.core.disable_wake().map_err(to_py)
    }

    #[getter]
    fn is_wake_enabled(&self) -> bool {
        self.core.is_wake_enabled()
    }

    #[getter]
    fn wake_alert(&self) -> Option<String> {
        self.core.wake_alert()
    }

    /// How sure the detector was of one word, most recently. Every
    /// score, because a threshold is guesswork without the near misses.
    fn wake_score(&self, word: &str) -> PyResult<Option<f32>> {
        self.core.wake_score(word).map_err(to_py)
    }

    /// Forget what has been heard, so a fresh utterance is needed.
    fn reset_wake(&self) -> PyResult<()> {
        self.core.reset_wake().map_err(to_py)
    }

    /// The threshold for every word not given one of its own.
    fn set_wake_threshold(&self, value: f32) -> PyResult<()> {
        self.core.set_wake_threshold(value).map_err(to_py)
    }

    /// Give one word its own threshold, or `None` to share the common one.
    fn set_wake_word_threshold(&self, word: &str, value: Option<f32>) -> PyResult<()> {
        self.core
            .set_wake_word_threshold(word, value)
            .map_err(to_py)
    }

    /// The threshold one word is held to, its own or the shared one.
    fn wake_word_threshold(&self, word: &str) -> PyResult<f32> {
        self.core.wake_word_threshold(word).map_err(to_py)
    }

    /// Frames of 80 ms to look away for after hearing the wake word.
    fn set_wake_settle_frames(&self, frames: u32) -> PyResult<()> {
        self.core.set_wake_settle_frames(frames).map_err(to_py)
    }

    // ── speech detection ─────────────────────────────────────────────

    fn enable_speech(&self) -> PyResult<()> {
        self.core.enable_speech().map_err(to_py)
    }

    fn disable_speech(&self) -> PyResult<()> {
        self.core.disable_speech().map_err(to_py)
    }

    #[getter]
    fn is_speech_enabled(&self) -> bool {
        self.core.is_speech_enabled()
    }

    fn start_recording(&self) -> PyResult<()> {
        self.core.start_recording().map_err(to_py)
    }

    fn stop_recording(&self) -> PyResult<()> {
        self.core.stop_recording().map_err(to_py)
    }

    #[getter]
    fn is_recording(&self) -> bool {
        self.core.is_recording()
    }

    // ── sound playback ───────────────────────────────────────────────

    /// Register a sound, from a file or from raw audio. For example
    /// `ear.register_sound("alert", path="alert.wav")`.
    #[pyo3(signature = (
        id, *, path = None, pcm = None,
        sample_rate = 16_000, channels = 1, sample_type = "i16", volume = 1.0
    ))]
    #[allow(clippy::too_many_arguments)]
    fn register_sound(
        &self,
        id: &str,
        path: Option<PathBuf>,
        pcm: Option<Bound<'_, PyAny>>,
        sample_rate: u32,
        channels: u16,
        sample_type: &str,
        volume: f32,
    ) -> PyResult<()> {
        let source = match (path, pcm) {
            (Some(path), None) => SoundSource::File { path },
            (None, Some(data)) => SoundSource::Pcm {
                // Raw audio carries no header, so the caller says what
                // the samples are and they are read that way.
                data: match sample_type_of(sample_type)? {
                    SampleType::I16 => Samples::I16(data.extract()?),
                    SampleType::F32 => Samples::F32(data.extract()?),
                },
                sample_rate,
                channels,
            },
            (Some(_), Some(_)) => {
                return Err(InvalidValue::new_err("give either path or pcm, not both"));
            }
            (None, None) => {
                return Err(InvalidValue::new_err("give either path or pcm"));
            }
        };
        self.core.register_sound(id, source, volume).map_err(to_py)
    }

    fn unregister_sound(&self, id: &str) -> PyResult<()> {
        self.core.unregister_sound(id).map_err(to_py)
    }

    #[pyo3(signature = (id, repeat = false))]
    fn play_sound(&self, id: &str, repeat: bool) -> PyResult<()> {
        self.core.play_sound(id, repeat).map_err(to_py)
    }

    fn stop_sound(&self) -> PyResult<()> {
        self.core.stop_sound().map_err(to_py)
    }

    #[getter]
    fn is_playing(&self) -> bool {
        self.core.is_playing()
    }

    // ── devices and settings ─────────────────────────────────────────

    fn input_devices(&self) -> PyResult<Vec<Device>> {
        Ok(self
            .core
            .input_devices()
            .map_err(to_py)?
            .into_iter()
            .map(|d| Device {
                id: d.id,
                name: d.name,
                is_default: d.is_default,
            })
            .collect())
    }

    /// Open the microphone at this. None goes back to its default.
    #[pyo3(signature = (sample_rate = None, channels = 1, sample_type = "i16"))]
    fn set_input_device_format(
        &self,
        sample_rate: Option<u32>,
        channels: u16,
        sample_type: &str,
    ) -> PyResult<()> {
        let wanted = wanted_format(sample_rate, channels, sample_type)?;
        self.core.set_input_device_format(wanted).map_err(to_py)
    }

    /// Open the speaker at this. Fixed once the first sound opens it.
    #[pyo3(signature = (sample_rate = None, channels = 1, sample_type = "i16"))]
    fn set_output_device_format(
        &self,
        sample_rate: Option<u32>,
        channels: u16,
        sample_type: &str,
    ) -> PyResult<()> {
        let wanted = wanted_format(sample_rate, channels, sample_type)?;
        self.core.set_output_device_format(wanted).map_err(to_py)
    }

    /// What the microphone opened at, or None before capture starts.
    #[getter]
    fn input_format(&self) -> Option<SupportedFormat> {
        self.core.input_format().map(opened_format)
    }

    /// What the speaker opened at, or None before a sound opens it.
    #[getter]
    fn output_format(&self) -> Option<SupportedFormat> {
        self.core.output_format().map(opened_format)
    }

    #[pyo3(signature = (device = None))]
    fn input_device_formats(&self, device: Option<&str>) -> PyResult<Vec<SupportedFormat>> {
        formats_of(self.core.input_device_formats(device))
    }

    #[pyo3(signature = (device = None))]
    fn output_device_formats(&self, device: Option<&str>) -> PyResult<Vec<SupportedFormat>> {
        formats_of(self.core.output_device_formats(device))
    }

    fn output_devices(&self) -> PyResult<Vec<Device>> {
        Ok(self
            .core
            .output_devices()
            .map_err(to_py)?
            .into_iter()
            .map(|d| Device {
                id: d.id,
                name: d.name,
                is_default: d.is_default,
            })
            .collect())
    }

    #[pyo3(signature = (id = None))]
    fn set_input_device(&self, id: Option<&str>) -> PyResult<()> {
        self.core.set_input_device(id).map_err(to_py)
    }

    #[pyo3(signature = (id = None))]
    fn set_output_device(&self, id: Option<&str>) -> PyResult<()> {
        self.core.set_output_device(id).map_err(to_py)
    }

    /// Set the audio one consumer receives. `target` is "read", "wake",
    /// or "speech".
    #[pyo3(signature = (target, sample_rate, channels = 1, sample_type = "i16"))]
    fn set_format(
        &self,
        target: &str,
        sample_rate: u32,
        channels: u16,
        sample_type: &str,
    ) -> PyResult<()> {
        let format = AudioFormat::new(sample_rate, channels, sample_type_of(sample_type)?);
        self.core
            .set_format(target_of(target)?, format)
            .map_err(to_py)
    }

    fn set_speech_threshold(&self, value: f32) -> PyResult<()> {
        self.core.set_speech_threshold(value).map_err(to_py)
    }

    fn set_silence_duration(&self, seconds: f64) -> PyResult<()> {
        self.core
            .set_silence_duration(Duration::from_secs_f64(seconds))
            .map_err(to_py)
    }

    fn set_max_recording(&self, seconds: f64) -> PyResult<()> {
        self.core
            .set_max_recording(Duration::from_secs_f64(seconds))
            .map_err(to_py)
    }

    fn set_no_speech_timeout(&self, seconds: f64) -> PyResult<()> {
        self.core
            .set_no_speech_timeout(Duration::from_secs_f64(seconds))
            .map_err(to_py)
    }

    fn set_pre_roll(&self, seconds: f64) -> PyResult<()> {
        self.core
            .set_pre_roll(Duration::from_secs_f64(seconds))
            .map_err(to_py)
    }

    fn set_wake_recording_waits_for_alert(&self, waits: bool) -> PyResult<()> {
        self.core
            .set_wake_recording_waits_for_alert(waits)
            .map_err(to_py)
    }

    fn set_ring_capacity(&self, seconds: f64) -> PyResult<()> {
        self.core
            .set_ring_capacity(Duration::from_secs_f64(seconds))
            .map_err(to_py)
    }

    fn __repr__(&self) -> String {
        format!("EdgeEar(running={})", self.core.is_running())
    }
}

impl Drop for EdgeEar {
    fn drop(&mut self) {
        // Reached with the interpreter lock held, and releasing the
        // devices waits on threads that log, so let go of it first.
        Python::try_attach(|py| py.detach(|| self.core.destroy()));
    }
}

fn wanted_format(
    sample_rate: Option<u32>,
    channels: u16,
    sample_type: &str,
) -> PyResult<Option<AudioFormat>> {
    let Some(rate) = sample_rate else {
        return Ok(None);
    };
    Ok(Some(AudioFormat::new(
        rate,
        channels,
        sample_type_of(sample_type)?,
    )))
}

/// An open device runs at one rate, so both ends of the span are it.
fn opened_format(found: AudioFormat) -> SupportedFormat {
    SupportedFormat {
        channels: found.channels,
        min_sample_rate: found.sample_rate,
        max_sample_rate: found.sample_rate,
        sample_type: match found.sample_type {
            SampleType::I16 => "i16".to_string(),
            SampleType::F32 => "f32".to_string(),
        },
    }
}

fn formats_of(
    found: Result<Vec<edge_ear_core::backend::SupportedFormat>, Error>,
) -> PyResult<Vec<SupportedFormat>> {
    Ok(found
        .map_err(to_py)?
        .into_iter()
        .map(|f| SupportedFormat {
            channels: f.channels,
            min_sample_rate: f.min_sample_rate,
            max_sample_rate: f.max_sample_rate,
            sample_type: match f.sample_type {
                SampleType::I16 => "i16".to_string(),
                SampleType::F32 => "f32".to_string(),
            },
        })
        .collect())
}

fn sample_type_of(name: &str) -> PyResult<SampleType> {
    sample_type(name)
}

fn target_of(name: &str) -> PyResult<Target> {
    target(name)
}

#[pymodule]
fn edge_ear(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<EdgeEar>()?;
    m.add_class::<AudioChunk>()?;
    m.add_class::<Device>()?;
    m.add_class::<SupportedFormat>()?;
    m.add_class::<AudioEvent>()?;
    m.add_class::<WakeDetected>()?;
    m.add_class::<SpeechEnded>()?;
    m.add_class::<SoundFinished>()?;
    m.add_class::<DeviceError>()?;
    m.add_class::<EventsDropped>()?;
    m.add_class::<EndReasonNames>()?;

    let py = m.py();
    install_logging(py)?;
    m.add_function(wrap_pyfunction!(reset_logging, m)?)?;
    m.add("EdgeEarError", py.get_type::<EdgeEarError>())?;
    m.add("NotRunning", py.get_type::<NotRunning>())?;
    m.add("AlreadyRunning", py.get_type::<AlreadyRunning>())?;
    m.add("RunningNotAllowed", py.get_type::<RunningNotAllowed>())?;
    m.add("RecordingOpen", py.get_type::<RecordingOpen>())?;
    m.add("Destroyed", py.get_type::<Destroyed>())?;
    m.add("NoWakeModel", py.get_type::<NoWakeModel>())?;
    m.add("ModelNotFound", py.get_type::<ModelNotFound>())?;
    m.add("ModelUnreadable", py.get_type::<ModelUnreadable>())?;
    m.add("ModelInvalid", py.get_type::<ModelInvalid>())?;
    m.add("UnsupportedFormat", py.get_type::<UnsupportedFormat>())?;
    m.add("InvalidValue", py.get_type::<InvalidValue>())?;
    m.add("NoDevice", py.get_type::<NoDevice>())?;
    m.add("PermissionDenied", py.get_type::<PermissionDenied>())?;
    m.add("DeviceLost", py.get_type::<DeviceLost>())?;
    m.add("UnknownSound", py.get_type::<UnknownSound>())?;
    m.add("UnknownWakeWord", py.get_type::<UnknownWakeWord>())?;
    m.add("Timeout", py.get_type::<Timeout>())?;
    m.add("Stopped", py.get_type::<Stopped>())?;
    m.add("BackendError", py.get_type::<BackendError>())?;
    m.add("ConversionError", py.get_type::<ConversionError>())?;
    Ok(())
}
