# Changelog

All notable changes to this project are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
versioning follows [SemVer](https://semver.org/); before 1.0.0, any
0.y release may break the public API.

## [Unreleased]

### Added

- core logs through the `log` crate: devices opening, capture starting
  and stopping, the wake word, recordings ending, and failures in the
  capture, detection and playback threads. Nothing is logged per audio
  block, and a repeating failure is logged once.
- A worker thread that panics now says so, rather than leaving the
  audio to stop with nothing to explain it. A failed start says what
  refused it, on the PipeWire backend as on cpal.
- py: what core logs arrives through Python's `logging`, on loggers
  under `edge_ear`. Levels are remembered so the audio threads stay off
  the interpreter lock; `edge_ear.reset_logging()` drops what was
  remembered when the Python side changes them.
- capi: `edge_ear_set_log_cb` hands what the library logs to the
  application, with the level to stop at. It is not tied to a handle,
  because one log covers the process, and it is called from whichever
  thread logged. A new `EDGE_EAR_LOG_TAKEN` says something else in the
  process already takes those messages.

### Fixed

- py: stopping or closing a handle held the interpreter lock while it
  waited for the threads it was stopping. A notification handler, or
  anything else those threads needed Python for, could not run until
  the wait ended, and the wait ended only once it had.

## [0.4.0]

### Added

- A PipeWire backend, `tinypipewire-backend`, over `tinypipewire-rs`.
  Devices are named and numbered by the PipeWire graph rather than by
  ALSA. Linux only, and off by default.
- `cpal-backend` and `tinypipewire-backend` features on `capi` and
  `py`, so a C or Python build chooses its own device backend. Both
  still default to `cpal-backend`. A crate inheriting `edge-ear-core`
  from the workspace now gets no backend unless it names one, because
  the workspace dependency no longer carries the default.

### Changed

- capi: `edge_ear_on_event` is now `edge_ear_set_event_cb`. It only
  ever held one handler at a time, replacing it on every call, so
  `on_` overstated it: nothing here fans out to multiple listeners
  (breaking).

## [0.3.0]

### Changed

- capi: getters are now named `edge_ear_get_*`. Renamed:
  `edge_ear_last_error`, `edge_ear_wake_score`, `edge_ear_wake_alert`,
  `edge_ear_input_format`, `edge_ear_output_format`,
  `edge_ear_input_device_formats`, `edge_ear_output_device_formats`,
  `edge_ear_input_devices`, `edge_ear_output_devices` (breaking).
  Boolean `is_*` queries are unchanged.

## [0.2.0]

0.1.0 was never tagged, so this entry covers everything built since
the workspace was scaffolded.

### Added

- Live microphone capture and speaker playback, with per-consumer
  audio formats and device enumeration.
- Wake word detection, with a configurable threshold, settle time,
  and an alert sound tied into the recording it opens.
- Speech and silence detection, with pre-roll, a maximum recording
  length, and a no-speech timeout.
- Sound registration and playback from files or raw PCM, including
  looped playback.
- A C API covering the full surface, and a Python binding.
- macOS support, verified end to end including a soak test.

### Changed

- capi: `edge_ear_h` is now a pointer typedef to an opaque
  `edge_ear_handle` struct. Every function takes `edge_ear_h ear`
  instead of `edge_ear_h *ear` (breaking).

## [0.1.0]

Workspace scaffolding: the Cargo workspace, licensing, and the
initial README.
