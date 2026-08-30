# Changelog

All notable changes to this project are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
versioning follows [SemVer](https://semver.org/); before 1.0.0, any
0.y release may break the public API.

## [Unreleased]

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
