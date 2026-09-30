//! C API for edge-ear. Binds the core crate directly, adding nothing.
//! One promise covers every call: handles come from `edge_ear_new`
//! unfreed, pointers are good for the call, strings are terminated.
#![allow(clippy::missing_safety_doc)]

mod convert;
mod error;
mod events;
mod logging;

use std::ffi::{CString, c_char, c_void};
use std::path::Path;
use std::sync::Mutex;

use edge_ear_core::EdgeEar;
use edge_ear_core::Samples;
use edge_ear_core::SoundSource;
use edge_ear_core::config::{AudioFormat, SampleType, Target};

use convert::{duration, optional_str, out_ptr, required_str};
use error::*;
use events::{Registered, edge_ear_event_cb};
use logging::{edge_ear_log_cb, edge_ear_log_level};

/// What a handle points to. Opaque on the C side, which only ever
/// names the pointer to this: `edge_ear_h`.
#[allow(non_camel_case_types)]
pub struct edge_ear_handle {
    core: EdgeEar,
    /// Strings handed out by name, kept alive until the next call that
    /// replaces them. Nothing here is ever freed by the caller.
    borrowed: Mutex<Borrowed>,
}

/// The handle a C caller holds.
#[allow(non_camel_case_types)]
pub type edge_ear_h = *mut edge_ear_handle;

#[derive(Default)]
struct Borrowed {
    alert: Option<CString>,
    wake_names: Vec<CString>,
    wake_listed: Vec<*const c_char>,
    devices: Vec<CString>,
    listed: Vec<edge_ear_device>,
    formats: Vec<edge_ear_format>,
}

impl edge_ear_handle {
    /// Keep the alert name alive for the caller to read.
    fn remember_alert(&self, name: Option<String>) -> *const c_char {
        let mut held = self.borrowed.lock().unwrap_or_else(|e| e.into_inner());
        match name.and_then(|n| CString::new(n).ok()) {
            Some(text) => {
                held.alert = Some(text);
                held.alert.as_ref().map_or(std::ptr::null(), |t| t.as_ptr())
            }
            None => {
                held.alert = None;
                std::ptr::null()
            }
        }
    }
}

/// Run a body against a handle, or report a null one.
macro_rules! with {
    ($ear:expr, $name:ident => $body:expr) => {{
        if $ear.is_null() {
            return fail_with(EDGE_EAR_NULL_ARGUMENT, "the handle must not be null");
        }
        let $name = unsafe { &*$ear };
        $body
    }};
}

/// Unwrap a converted argument, or return the code it failed with.
macro_rules! ok_or_return {
    ($result:expr) => {
        match $result {
            Ok(value) => value,
            Err(code) => return code,
        }
    };
}

// ---- errors ----------------------------------------------------

/// @brief The message behind the last failing call on this thread.
///
/// @return The message, borrowed until the next call on this thread
///         fails. Empty when nothing has failed yet.
#[unsafe(no_mangle)]
pub extern "C" fn edge_ear_get_last_error() -> *const c_char {
    last_message()
}

// ---- logging ---------------------------------------------------

/// @brief Send what the library logs to this callback.
///
/// Not tied to a handle: one callback takes the messages of the whole
/// process, and setting another replaces it. It is called from
/// whichever thread logged, including the ones reading the microphone
/// and running the models, so it must stand being called from several
/// at once and must return promptly. Nothing is logged per block of
/// audio, and a repeating failure is logged once.
///
/// @param[in] callback where the messages go, or NULL to stop sending
///            them
/// @param[in] level how far down to go; the rest is dropped before it
///            is even written out
/// @param[in] user passed to the callback untouched; the caller keeps
///            it alive until the callback is replaced or removed
/// @return #EDGE_EAR_OK, or #EDGE_EAR_LOG_TAKEN when something else in
///         this process already takes these messages.
#[unsafe(no_mangle)]
pub extern "C" fn edge_ear_set_log_cb(
    callback: edge_ear_log_cb,
    level: edge_ear_log_level,
    user: *mut c_void,
) -> i32 {
    if logging::point_at(callback, level, user) {
        OK
    } else {
        fail_with(
            EDGE_EAR_LOG_TAKEN,
            "something else in this process already takes the log",
        )
    }
}

// ---- handle ----------------------------------------------------

/// @brief Make a handle.
///
/// @return The handle, or NULL when no device could be reached. On
///         NULL, edge_ear_get_last_error() says why.
/// @see edge_ear_free
#[unsafe(no_mangle)]
pub extern "C" fn edge_ear_new() -> edge_ear_h {
    match EdgeEar::new() {
        Ok(core) => Box::into_raw(Box::new(edge_ear_handle {
            core,
            borrowed: Mutex::default(),
        })),
        Err(e) => {
            fail(&e);
            std::ptr::null_mut()
        }
    }
}

/// @brief Release the handle and everything it owns.
///
/// Must not run alongside another call on the same handle, or from
/// inside a handler.
///
/// @param[in] ear the handle, or NULL to do nothing
/// @see edge_ear_new
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_free(ear: edge_ear_h) {
    if ear.is_null() {
        return;
    }
    let owned = unsafe { Box::from_raw(ear) };
    owned.core.destroy();
}

/// @brief Open the microphone and begin reading.
///
/// Formats and devices are fixed from here until the handle is stopped.
///
/// @param[in] ear the handle
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_stop, edge_ear_read
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_start(ear: edge_ear_h) -> i32 {
    with!(ear, e => report(e.core.start()))
}

/// @brief Release the microphone and the speaker.
///
/// The handle can be set up and started again afterwards.
///
/// @param[in] ear the handle
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_start
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_stop(ear: edge_ear_h) -> i32 {
    with!(ear, e => report(e.core.stop()))
}

/// @brief Whether capture is running.
///
/// @param[in] ear the handle
/// @return 1 while running, 0 when not, #EDGE_EAR_NULL_ARGUMENT for a
///         null handle.
/// @see edge_ear_start
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_is_running(ear: edge_ear_h) -> i32 {
    with!(ear, e => i32::from(e.core.is_running()))
}

// ---- reading audio ---------------------------------------------

/// @brief Take the next block of live audio.
///
/// A block larger than `cap` is refused rather than truncated, so
/// nothing is ever lost without being told.
///
/// @param[in] ear the handle
/// @param[out] buf where the samples go
/// @param[in]  cap how many samples fit in `buf`
/// @param[in]  timeout_ms how long to wait; below zero waits until
///             audio arrives or capture stops
/// @param[out] got how many samples were written
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_set_format
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_read(
    ear: edge_ear_h,
    buf: *mut i16,
    cap: usize,
    timeout_ms: i32,
    got: *mut usize,
) -> i32 {
    with!(ear, e => {
        let got = ok_or_return!(out_ptr(got, "got"));
        if buf.is_null() {
            return fail_with(EDGE_EAR_NULL_ARGUMENT, "the buffer must not be null");
        }
        let wait = (timeout_ms >= 0)
            .then(|| std::time::Duration::from_millis(timeout_ms as u64));

        let chunk = match e.core.read(wait) {
            Ok(chunk) => chunk,
            Err(err) => return fail(&err),
        };
        let Some(samples) = chunk.samples.as_i16() else {
            return fail_with(
                EDGE_EAR_UNSUPPORTED_FORMAT,
                "the read format is not 16-bit; set it before starting",
            );
        };
        if samples.len() > cap {
            *got = 0;
            return fail_with(
                EDGE_EAR_INVALID_VALUE,
                &format!("the buffer holds {cap} samples, the block has {}", samples.len()),
            );
        }
        // Safe: the caller promised `cap` samples of room and the
        // length was just checked against it.
        unsafe { std::ptr::copy_nonoverlapping(samples.as_ptr(), buf, samples.len()) };
        *got = samples.len();
        OK
    })
}

// ---- notifications ---------------------------------------------

/// @brief Set the handler called for every notification.
///
/// It runs on the one thread that delivers notifications, so a slow
/// handler holds up later notifications only. It may call back in.
///
/// @param[in] ear the handle
/// @param[in] callback the handler, or NULL to remove the current one
/// @param[in] user passed to the handler untouched; the caller keeps it
///            alive until the handle is freed
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_set_event_cb(
    ear: edge_ear_h,
    callback: edge_ear_event_cb,
    user: *mut c_void,
) -> i32 {
    with!(ear, e => {
        let registered = Registered { callback, user };
        report(e.core.on_event(move |event| registered.deliver(event)))
    })
}

// ---- wake word --------------------------------------------------

/// @brief Supply the two models every wake word shares.
///
/// Neither knows any word, and neither ships with this library.
///
/// @param[in] ear the handle
/// @param[in] spectrogram path to the melspectrogram model
/// @param[in] features path to the speech embedding model
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_add_wake_model
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_load_wake_features(
    ear: edge_ear_h,
    spectrogram: *const c_char,
    features: *const c_char,
) -> i32 {
    with!(ear, e => {
        let one = ok_or_return!(required_str(spectrogram, "the spectrogram path"));
        let two = ok_or_return!(required_str(features, "the features path"));
        report(e.core.load_wake_features(Path::new(one), Path::new(two)))
    })
}

/// @brief Add a phrase to listen for, under a name.
///
/// Several can be listened for at once, and the two shared models run
/// once for all of them. A detection carries the name of the word
/// heard. Adding under a name already in use replaces that word. Its
/// shape is checked here, and a model built for another pipeline is
/// refused by name of what was wrong.
///
/// @param[in] ear the handle
/// @param[in] name what detections of this word are called, or NULL to
///            call it after its file
/// @param[in] path path to the wake word model
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_load_wake_features, edge_ear_enable_wake
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_add_wake_model(
    ear: edge_ear_h,
    name: *const c_char,
    path: *const c_char,
) -> i32 {
    with!(ear, e => {
        let name = ok_or_return!(optional_str(name, "the wake word name"));
        let path = ok_or_return!(required_str(path, "the model path"));
        report(e.core.add_wake_model(name, Path::new(path)))
    })
}

/// @brief Stop listening for one phrase and forget its model.
///
/// @param[in] ear the handle
/// @param[in] name the name it was added under
/// @return #EDGE_EAR_OK, #EDGE_EAR_UNKNOWN_WAKE_WORD, or another
///         negative #edge_ear_error.
/// @see edge_ear_add_wake_model
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_remove_wake_model(ear: edge_ear_h, name: *const c_char) -> i32 {
    with!(ear, e => {
        let name = ok_or_return!(required_str(name, "the wake word name"));
        report(e.core.remove_wake_model(name))
    })
}

/// @brief The names of the words listened for, in the order added.
///
/// @param[in] ear the handle
/// @param[out] names where the list goes, borrowed until the next call
///             to this function on this handle
/// @param[out] count how many names the list holds
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_add_wake_model
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_get_wake_models(
    ear: edge_ear_h,
    names: *mut *const *const c_char,
    count: *mut usize,
) -> i32 {
    with!(ear, e => {
        let names = ok_or_return!(out_ptr(names, "names"));
        let count = ok_or_return!(out_ptr(count, "count"));
        let mut held = e.borrowed.lock().unwrap_or_else(|e| e.into_inner());
        held.wake_names = e
            .core
            .wake_models()
            .into_iter()
            .filter_map(|n| CString::new(n).ok())
            .collect();
        held.wake_listed = held.wake_names.iter().map(|n| n.as_ptr()).collect();
        *names = held.wake_listed.as_ptr();
        *count = held.wake_listed.len();
        OK
    })
}

/// @brief Start listening for the wake word.
///
/// @param[in] ear the handle
/// @param[in] alert a registered sound to play on detection, or NULL.
///            Naming one also holds off counting silence until it ends,
///            so its tail is not taken for speech
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_disable_wake, edge_ear_register_sound_file
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_enable_wake(ear: edge_ear_h, alert: *const c_char) -> i32 {
    with!(ear, e => {
        let alert = ok_or_return!(optional_str(alert, "the alert name"));
        report(e.core.enable_wake(alert))
    })
}

/// @brief Stop listening for the wake word.
///
/// The models stay loaded.
///
/// @param[in] ear the handle
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_enable_wake
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_disable_wake(ear: edge_ear_h) -> i32 {
    with!(ear, e => report(e.core.disable_wake()))
}

/// @brief Whether the wake word is being listened for.
///
/// @param[in] ear the handle
/// @return 1 when listening, 0 when not, #EDGE_EAR_NULL_ARGUMENT for a
///         null handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_is_wake_enabled(ear: edge_ear_h) -> i32 {
    with!(ear, e => i32::from(e.core.is_wake_enabled()))
}

/// @brief Clear what the detector has heard and look away.
///
/// The same as if the word had just been heard.
///
/// @param[in] ear the handle
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_set_wake_settle_frames
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_reset_wake(ear: edge_ear_h) -> i32 {
    with!(ear, e => report(e.core.reset_wake()))
}

/// @brief How sure the detector was of one word, most recently.
///
/// Every score, not only the ones that counted, because choosing a
/// threshold is guesswork without seeing the near misses.
///
/// @param[in] ear the handle
/// @param[in] word the name the word was added under
/// @param[out] score where the score goes, from 0.0 to 1.0
/// @return #EDGE_EAR_OK, #EDGE_EAR_NOT_RUNNING when nothing has scored
///         yet, or #EDGE_EAR_UNKNOWN_WAKE_WORD.
/// @see edge_ear_set_wake_word_threshold
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_get_wake_score(
    ear: edge_ear_h,
    word: *const c_char,
    score: *mut f32,
) -> i32 {
    with!(ear, e => {
        let word = ok_or_return!(required_str(word, "the wake word name"));
        let score = ok_or_return!(out_ptr(score, "score"));
        match e.core.wake_score(word) {
            Ok(Some(value)) => { *score = value; OK }
            Ok(None) => fail_with(EDGE_EAR_NOT_RUNNING, "nothing has been scored yet"),
            Err(err) => fail(&err),
        }
    })
}

/// @brief The sound played on detection.
///
/// @param[in] ear the handle
/// @param[out] alert where the name goes, or NULL when none was named.
///             Borrowed until the next call replaces it
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_enable_wake
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_get_wake_alert(
    ear: edge_ear_h,
    alert: *mut *const c_char,
) -> i32 {
    with!(ear, e => {
        let out = ok_or_return!(out_ptr(alert, "alert"));
        *out = e.remember_alert(e.core.wake_alert());
        OK
    })
}

/// @brief How sure the detector must be before it says it heard.
///
/// Applies to every word not given a threshold of its own. 0.5 by
/// default.
///
/// @param[in] ear the handle
/// @param[in] value from 0.0 to 1.0
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_set_wake_word_threshold, edge_ear_get_wake_score
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_set_wake_threshold(ear: edge_ear_h, value: f32) -> i32 {
    with!(ear, e => report(e.core.set_wake_threshold(value)))
}

/// @brief Give one word a threshold of its own.
///
/// Changeable while running.
///
/// @param[in] ear the handle
/// @param[in] word the name the word was added under
/// @param[in] value from 0.0 to 1.0
/// @return #EDGE_EAR_OK, #EDGE_EAR_UNKNOWN_WAKE_WORD, or another
///         negative #edge_ear_error.
/// @see edge_ear_unset_wake_word_threshold, edge_ear_set_wake_threshold
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_set_wake_word_threshold(
    ear: edge_ear_h,
    word: *const c_char,
    value: f32,
) -> i32 {
    with!(ear, e => {
        let word = ok_or_return!(required_str(word, "the wake word name"));
        report(e.core.set_wake_word_threshold(word, Some(value)))
    })
}

/// @brief Hold one word to the shared threshold again.
///
/// @param[in] ear the handle
/// @param[in] word the name the word was added under
/// @return #EDGE_EAR_OK, #EDGE_EAR_UNKNOWN_WAKE_WORD, or another
///         negative #edge_ear_error.
/// @see edge_ear_set_wake_word_threshold
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_unset_wake_word_threshold(
    ear: edge_ear_h,
    word: *const c_char,
) -> i32 {
    with!(ear, e => {
        let word = ok_or_return!(required_str(word, "the wake word name"));
        report(e.core.set_wake_word_threshold(word, None))
    })
}

/// @brief The threshold one word is held to, its own or the shared one.
///
/// @param[in] ear the handle
/// @param[in] word the name the word was added under
/// @param[out] value where the threshold goes
/// @return #EDGE_EAR_OK, #EDGE_EAR_UNKNOWN_WAKE_WORD, or another
///         negative #edge_ear_error.
/// @see edge_ear_set_wake_word_threshold
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_get_wake_word_threshold(
    ear: edge_ear_h,
    word: *const c_char,
    value: *mut f32,
) -> i32 {
    with!(ear, e => {
        let word = ok_or_return!(required_str(word, "the wake word name"));
        let value = ok_or_return!(out_ptr(value, "value"));
        match e.core.wake_word_threshold(word) {
            Ok(found) => { *value = found; OK }
            Err(err) => fail(&err),
        }
    })
}

/// @brief How long to look away after hearing the word.
///
/// Long enough that the same words are not heard twice on their way out
/// of the pipeline. 20 frames by default.
///
/// @param[in] ear the handle
/// @param[in] frames how many frames of 80 ms to ignore
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_reset_wake
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_set_wake_settle_frames(ear: edge_ear_h, frames: u32) -> i32 {
    with!(ear, e => report(e.core.set_wake_settle_frames(frames)))
}

// ---- speech and recording ---------------------------------------

/// @brief Start watching for the end of speech.
///
/// Takes effect on the next block of audio.
///
/// @param[in] ear the handle
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_disable_speech, edge_ear_start_recording
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_enable_speech(ear: edge_ear_h) -> i32 {
    with!(ear, e => report(e.core.enable_speech()))
}

/// @brief Stop watching for speech.
///
/// Any open recording is dropped.
///
/// @param[in] ear the handle
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_enable_speech
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_disable_speech(ear: edge_ear_h) -> i32 {
    with!(ear, e => report(e.core.disable_speech()))
}

/// @brief Whether speech and silence are being watched for.
///
/// @param[in] ear the handle
/// @return 1 when watching, 0 when not, #EDGE_EAR_NULL_ARGUMENT for a
///         null handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_is_speech_enabled(ear: edge_ear_h) -> i32 {
    with!(ear, e => i32::from(e.core.is_speech_enabled()))
}

/// @brief Open a recording.
///
/// It ends on silence, on the no-speech timeout, at the length cap, or
/// when stopped, and the audio arrives as a notification.
///
/// @param[in] ear the handle
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_stop_recording, edge_ear_set_pre_roll
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_start_recording(ear: edge_ear_h) -> i32 {
    with!(ear, e => report(e.core.start_recording()))
}

/// @brief End the open recording.
///
/// One notification follows, saying it was stopped rather than that the
/// speaker went quiet.
///
/// @param[in] ear the handle
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_start_recording
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_stop_recording(ear: edge_ear_h) -> i32 {
    with!(ear, e => report(e.core.stop_recording()))
}

/// @brief Whether a recording is collecting audio.
///
/// @param[in] ear the handle
/// @return 1 while recording, 0 when not, #EDGE_EAR_NULL_ARGUMENT for a
///         null handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_is_recording(ear: edge_ear_h) -> i32 {
    with!(ear, e => i32::from(e.core.is_recording()))
}

/// @brief How readily audio counts as speech.
///
/// 0.5 by default.
///
/// @param[in] ear the handle
/// @param[in] value from 0.0 to 1.0
/// @return #EDGE_EAR_OK or a negative #edge_ear_error. Taken on the
///         next frame, so an open recording follows the new value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_set_speech_threshold(ear: edge_ear_h, value: f32) -> i32 {
    with!(ear, e => report(e.core.set_speech_threshold(value)))
}

/// @brief How long the speaker must be quiet before a recording ends.
///
/// Three seconds by default.
///
/// @param[in] ear the handle
/// @param[in] seconds greater than zero
/// @return #EDGE_EAR_OK or a negative #edge_ear_error. Taken on the
///         next frame, so an open recording follows the new value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_set_silence_duration(ear: edge_ear_h, seconds: f64) -> i32 {
    with!(ear, e => {
        let span = ok_or_return!(duration(seconds, "the silence duration"));
        report(e.core.set_silence_duration(span))
    })
}

/// @brief The longest a recording may run before it is handed over.
///
/// Thirty seconds by default.
///
/// @param[in] ear the handle
/// @param[in] seconds greater than the silence duration
/// @return #EDGE_EAR_OK or a negative #edge_ear_error. Taken on the
///         next frame, so an open recording follows the new value.
/// @see edge_ear_set_silence_duration
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_set_max_recording(ear: edge_ear_h, seconds: f64) -> i32 {
    with!(ear, e => {
        let span = ok_or_return!(duration(seconds, "the maximum recording length"));
        report(e.core.set_max_recording(span))
    })
}

/// @brief How long to wait for anyone to speak at all.
///
/// Ten seconds by default.
///
/// @param[in] ear the handle
/// @param[in] seconds greater than zero
/// @return #EDGE_EAR_OK or a negative #edge_ear_error. Taken on the
///         next frame, so an open recording follows the new value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_set_no_speech_timeout(ear: edge_ear_h, seconds: f64) -> i32 {
    with!(ear, e => {
        let span = ok_or_return!(duration(seconds, "the no-speech timeout"));
        report(e.core.set_no_speech_timeout(span))
    })
}

/// @brief How much audio from before the recording to include.
///
/// So a word begun early is not cut off. Used by a recording the wake
/// word opened as much as by one this application asked for. Zero by
/// default, so no pre-roll audio is added until this is called.
///
/// @param[in] ear the handle
/// @param[in] seconds no more than the queue capacity
/// @return #EDGE_EAR_OK, #EDGE_EAR_RECORDING_OPEN while a recording is
///         open, or another negative #edge_ear_error.
/// @see edge_ear_set_ring_capacity,
///      edge_ear_set_wake_recording_waits_for_alert
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_set_pre_roll(ear: edge_ear_h, seconds: f64) -> i32 {
    with!(ear, e => {
        let span = ok_or_return!(duration(seconds, "the pre-roll"));
        report(e.core.set_pre_roll(span))
    })
}

/// @brief Whether a wake word recording begins again when the alert
///        ends.
///
/// Off by default, so the recording collects from the moment the wake
/// word lands and the microphone hears the alert into it. Turning it
/// on leaves the alert behind, at the price of a speaker who talks
/// over it, and puts the pre-roll out of reach on that path.
///
/// @param[in] ear the handle
/// @param[in] waits non-zero to wait for the alert
/// @return #EDGE_EAR_OK, #EDGE_EAR_RECORDING_OPEN while a recording is
///         open, or another negative #edge_ear_error.
/// @see edge_ear_enable_wake, edge_ear_set_pre_roll
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_set_wake_recording_waits_for_alert(
    ear: edge_ear_h,
    waits: i32,
) -> i32 {
    with!(ear, e => report(e.core.set_wake_recording_waits_for_alert(waits != 0)))
}

// ---- sounds ------------------------------------------------------

/// @brief Register a sound read from a file.
///
/// Decoding happens now, so playing it later is only a copy. Allowed
/// while running.
///
/// @param[in] ear the handle
/// @param[in] id the name to play it by later
/// @param[in] path a wav or ogg file
/// @param[in] volume from 0.0 to 1.0
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_play_sound, edge_ear_unregister_sound
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_register_sound_file(
    ear: edge_ear_h,
    id: *const c_char,
    path: *const c_char,
    volume: f32,
) -> i32 {
    with!(ear, e => {
        let id = ok_or_return!(required_str(id, "the sound name"));
        let path = ok_or_return!(required_str(path, "the sound path"));
        let source = SoundSource::File { path: path.into() };
        report(e.core.register_sound(id, source, volume))
    })
}

/// How one sample is written, as the C side names it.
fn sample_type_of(kind: edge_ear_sample_type) -> SampleType {
    match kind {
        edge_ear_sample_type::EDGE_EAR_SAMPLE_TYPE_I16 => SampleType::I16,
        edge_ear_sample_type::EDGE_EAR_SAMPLE_TYPE_F32 => SampleType::F32,
    }
}

/// @brief Register a sound from raw audio the caller already holds.
///
/// Raw audio carries no header, so `sample_rate`, `channels` and
/// `sample_type` say what `data` holds. The samples are copied, so they
/// may be freed once this returns.
///
/// @param[in] ear the handle
/// @param[in] id the name to play it by later
/// @param[in] data samples in the type named below
/// @param[in] len how many samples `data` holds, not bytes
/// @param[in] sample_rate the rate those samples were taken at
/// @param[in] channels 1 or 2
/// @param[in] sample_type how one sample is written
/// @param[in] volume from 0.0 to 1.0
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_play_sound, edge_ear_unregister_sound
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_register_sound_pcm(
    ear: edge_ear_h,
    id: *const c_char,
    data: *const c_void,
    len: usize,
    sample_rate: u32,
    channels: u16,
    sample_type: edge_ear_sample_type,
    volume: f32,
) -> i32 {
    with!(ear, e => {
        let id = ok_or_return!(required_str(id, "the sound name"));
        if data.is_null() {
            return fail_with(EDGE_EAR_NULL_ARGUMENT, "the audio must not be null");
        }
        let kind = sample_type_of(sample_type);
        // Safe: the caller promised `len` samples of `kind` at `data`.
        let samples = match kind {
            SampleType::I16 => {
                Samples::I16(unsafe { std::slice::from_raw_parts(data.cast::<i16>(), len) }.to_vec())
            }
            SampleType::F32 => {
                Samples::F32(unsafe { std::slice::from_raw_parts(data.cast::<f32>(), len) }.to_vec())
            }
        };
        let source = SoundSource::Pcm {
            data: samples,
            sample_rate,
            channels,
        };
        report(e.core.register_sound(id, source, volume))
    })
}

/// @brief Forget a registered sound.
///
/// One already playing is left to finish.
///
/// @param[in] ear the handle
/// @param[in] id the name it was registered under
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_register_sound_file
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_unregister_sound(ear: edge_ear_h, id: *const c_char) -> i32 {
    with!(ear, e => {
        let id = ok_or_return!(required_str(id, "the sound name"));
        report(e.core.unregister_sound(id))
    })
}

/// @brief Play a registered sound.
///
/// @param[in] ear the handle
/// @param[in] id the name it was registered under
/// @param[in] repeat non-zero loops it until stopped
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_stop_sound, edge_ear_is_playing
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_play_sound(
    ear: edge_ear_h,
    id: *const c_char,
    repeat: i32,
) -> i32 {
    with!(ear, e => {
        let id = ok_or_return!(required_str(id, "the sound name"));
        report(e.core.play_sound(id, repeat != 0))
    })
}

/// @brief Stop whatever is playing.
///
/// No finished notification follows, because the sound did not reach
/// its own end.
///
/// @param[in] ear the handle
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_play_sound
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_stop_sound(ear: edge_ear_h) -> i32 {
    with!(ear, e => report(e.core.stop_sound()))
}

/// @brief Whether a sound is coming out of the speaker.
///
/// Stays 1 until its last sample has been heard, not merely handed over.
///
/// @param[in] ear the handle
/// @return 1 while playing, 0 when not, #EDGE_EAR_NULL_ARGUMENT for a
///         null handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_is_playing(ear: edge_ear_h) -> i32 {
    with!(ear, e => i32::from(e.core.is_playing()))
}

// ---- devices -----------------------------------------------------

/// One device. Both strings are borrowed until the next listing call
/// on the same handle.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(non_camel_case_types)]
pub struct edge_ear_device {
    /// What to pass to `edge_ear_set_input_device`. Stable across runs.
    pub id: *const c_char,
    /// What to show a person. Two devices may share one.
    pub name: *const c_char,
    /// One when the system would pick this without being asked.
    pub is_default: i32,
}

/// One shape of audio a device says it will take.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct edge_ear_format {
    /// How many channels at this setting.
    pub channels: u16,
    /// The lowest rate it will take here.
    pub min_sample_rate: u32,
    /// The highest rate it will take here. Equal to the lowest when a
    /// device offers one rate rather than a span.
    pub max_sample_rate: u32,
    /// What this library hands over, or takes, at this setting.
    pub sample_type: edge_ear_sample_type,
}

fn list_formats(
    ear: &edge_ear_handle,
    found: edge_ear_core::error::Result<Vec<edge_ear_core::backend::SupportedFormat>>,
    formats: *mut *const edge_ear_format,
    count: *mut usize,
) -> i32 {
    let formats = match out_ptr(formats, "formats") {
        Ok(slot) => slot,
        Err(code) => return code,
    };
    let count = match out_ptr(count, "count") {
        Ok(slot) => slot,
        Err(code) => return code,
    };
    let found = match found {
        Ok(found) => found,
        Err(e) => return fail(&e),
    };

    let mut held = ear.borrowed.lock().unwrap_or_else(|e| e.into_inner());
    held.formats = found
        .iter()
        .map(|f| edge_ear_format {
            channels: f.channels,
            min_sample_rate: f.min_sample_rate,
            max_sample_rate: f.max_sample_rate,
            sample_type: match f.sample_type {
                SampleType::I16 => edge_ear_sample_type::EDGE_EAR_SAMPLE_TYPE_I16,
                SampleType::F32 => edge_ear_sample_type::EDGE_EAR_SAMPLE_TYPE_F32,
            },
        })
        .collect();
    *formats = held.formats.as_ptr();
    *count = held.formats.len();
    OK
}

fn list_devices(
    ear: &edge_ear_handle,
    found: edge_ear_core::error::Result<Vec<edge_ear_core::backend::DeviceInfo>>,
    devices: *mut *const edge_ear_device,
    count: *mut usize,
) -> i32 {
    let devices = match out_ptr(devices, "devices") {
        Ok(slot) => slot,
        Err(code) => return code,
    };
    let count = match out_ptr(count, "count") {
        Ok(slot) => slot,
        Err(code) => return code,
    };
    let found = match found {
        Ok(found) => found,
        Err(e) => return fail(&e),
    };

    let mut held = ear.borrowed.lock().unwrap_or_else(|e| e.into_inner());
    held.devices.clear();
    held.listed.clear();
    for device in &found {
        held.devices
            .push(CString::new(device.id.as_str()).unwrap_or_default());
        held.devices
            .push(CString::new(device.name.as_str()).unwrap_or_default());
    }
    // Pointers first: the strings they point at must not move again
    // before the list is built from them.
    let listed: Vec<edge_ear_device> = found
        .iter()
        .enumerate()
        .map(|(index, device)| edge_ear_device {
            id: held.devices[index * 2].as_ptr(),
            name: held.devices[index * 2 + 1].as_ptr(),
            is_default: i32::from(device.is_default),
        })
        .collect();
    held.listed = listed;
    *devices = held.listed.as_ptr();
    *count = held.listed.len();
    OK
}

/// @brief Open the microphone at this rather than at its default.
///
/// Refused here if the named device does not offer it, and again when
/// capture starts, because the device may have changed by then. Pass
/// zero for `sample_rate` to go back to the device's own choice.
///
/// @param[in] ear the handle
/// @param[in] sample_rate the rate to open at, or 0 for the default
/// @param[in] channels 1 or 2
/// @param[in] sample_type how one sample is written
/// @return #EDGE_EAR_OK, #EDGE_EAR_UNSUPPORTED_FORMAT when the device
///         does not offer it, or another negative #edge_ear_error.
/// @see edge_ear_get_input_device_formats, edge_ear_get_input_format
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_set_input_device_format(
    ear: edge_ear_h,
    sample_rate: u32,
    channels: u16,
    sample_type: edge_ear_sample_type,
) -> i32 {
    with!(ear, e => report(e.core.set_input_device_format(wanted_format(
        sample_rate, channels, sample_type,
    ))))
}

/// @brief Open the speaker at this rather than at its default.
///
/// Fixed once the speaker is open, which is when the first sound is
/// registered or played. Pass zero for `sample_rate` to go back to the
/// device's own choice.
///
/// @param[in] ear the handle
/// @param[in] sample_rate the rate to open at, or 0 for the default
/// @param[in] channels 1 or 2
/// @param[in] sample_type how one sample is written
/// @return #EDGE_EAR_OK, #EDGE_EAR_UNSUPPORTED_FORMAT when the device
///         does not offer it, #EDGE_EAR_RUNNING_NOT_ALLOWED once the
///         speaker is open, or another negative #edge_ear_error.
/// @see edge_ear_get_output_device_formats, edge_ear_get_output_format
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_set_output_device_format(
    ear: edge_ear_h,
    sample_rate: u32,
    channels: u16,
    sample_type: edge_ear_sample_type,
) -> i32 {
    with!(ear, e => report(e.core.set_output_device_format(wanted_format(
        sample_rate, channels, sample_type,
    ))))
}

/// @brief What the microphone opened at.
///
/// Not always what was asked for. The two rates in `format` are equal,
/// because an open device runs at one.
///
/// @param[in] ear the handle
/// @param[out] format where it goes
/// @return #EDGE_EAR_OK, #EDGE_EAR_NOT_RUNNING before capture starts,
///         or another negative #edge_ear_error.
/// @see edge_ear_set_input_device_format
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_get_input_format(
    ear: edge_ear_h,
    format: *mut edge_ear_format,
) -> i32 {
    with!(ear, e => opened_format(e.core.input_format(), format, EDGE_EAR_NOT_RUNNING))
}

/// @brief What the speaker opened at.
///
/// Not always what was asked for. The two rates in `format` are equal,
/// because an open device runs at one.
///
/// @param[in] ear the handle
/// @param[out] format where it goes
/// @return #EDGE_EAR_OK, #EDGE_EAR_NOT_RUNNING before the first sound
///         opens the speaker, or another negative #edge_ear_error.
/// @see edge_ear_set_output_device_format
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_get_output_format(
    ear: edge_ear_h,
    format: *mut edge_ear_format,
) -> i32 {
    with!(ear, e => opened_format(e.core.output_format(), format, EDGE_EAR_NOT_RUNNING))
}

/// A rate of zero asks for the device's own choice.
fn wanted_format(
    sample_rate: u32,
    channels: u16,
    sample_type: edge_ear_sample_type,
) -> Option<AudioFormat> {
    (sample_rate > 0).then(|| AudioFormat::new(sample_rate, channels, sample_type_of(sample_type)))
}

/// An open device runs at one rate, so both ends of the span are it.
fn opened_format(
    found: Option<AudioFormat>,
    out: *mut edge_ear_format,
    missing: edge_ear_error,
) -> i32 {
    let out = match out_ptr(out, "format") {
        Ok(slot) => slot,
        Err(code) => return code,
    };
    let Some(found) = found else {
        return fail_with(missing, "the device is not open");
    };
    *out = edge_ear_format {
        channels: found.channels,
        min_sample_rate: found.sample_rate,
        max_sample_rate: found.sample_rate,
        sample_type: match found.sample_type {
            SampleType::I16 => edge_ear_sample_type::EDGE_EAR_SAMPLE_TYPE_I16,
            SampleType::F32 => edge_ear_sample_type::EDGE_EAR_SAMPLE_TYPE_F32,
        },
    };
    OK
}

/// @brief What one microphone will take.
///
/// Rates come as a span, because that is how a device describes
/// itself. A device offering single rates reports each one with the
/// same low and high.
///
/// @param[in] ear the handle
/// @param[in] device the identifier, or NULL for the default one
/// @param[out] formats where the list goes, borrowed until the next
///             listing call on this handle
/// @param[out] count how many entries the list holds
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_get_input_devices, edge_ear_set_format
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_get_input_device_formats(
    ear: edge_ear_h,
    device: *const c_char,
    formats: *mut *const edge_ear_format,
    count: *mut usize,
) -> i32 {
    with!(ear, e => {
        let name = ok_or_return!(optional_str(device, "the device"));
        let found = e.core.input_device_formats(name);
        list_formats(e, found, formats, count)
    })
}

/// @brief What one speaker will take.
///
/// Sounds are converted to whichever of these the speaker is opened
/// at, so this says what to expect of them.
///
/// @param[in] ear the handle
/// @param[in] device the identifier, or NULL for the default one
/// @param[out] formats where the list goes, borrowed until the next
///             listing call on this handle
/// @param[out] count how many entries the list holds
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_get_output_devices, edge_ear_register_sound_pcm
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_get_output_device_formats(
    ear: edge_ear_h,
    device: *const c_char,
    formats: *mut *const edge_ear_format,
    count: *mut usize,
) -> i32 {
    with!(ear, e => {
        let name = ok_or_return!(optional_str(device, "the device"));
        let found = e.core.output_device_formats(name);
        list_formats(e, found, formats, count)
    })
}

/// @brief Every microphone the system offers.
///
/// @param[in] ear the handle
/// @param[out] devices where the list goes, borrowed until the next
///             listing call on this handle
/// @param[out] count how many devices the list holds
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_set_input_device
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_get_input_devices(
    ear: edge_ear_h,
    devices: *mut *const edge_ear_device,
    count: *mut usize,
) -> i32 {
    with!(ear, e => list_devices(e, e.core.input_devices(), devices, count))
}

/// @brief Every speaker the system offers.
///
/// @param[in] ear the handle
/// @param[out] devices where the list goes, borrowed until the next
///             listing call on this handle
/// @param[out] count how many devices the list holds
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_set_output_device
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_get_output_devices(
    ear: edge_ear_h,
    devices: *mut *const edge_ear_device,
    count: *mut usize,
) -> i32 {
    with!(ear, e => list_devices(e, e.core.output_devices(), devices, count))
}

/// @brief Choose a microphone.
///
/// The name is taken as given and checked when capture starts.
///
/// @param[in] ear the handle
/// @param[in] id an identifier from edge_ear_get_input_devices(), or NULL
///            for the system default
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_get_input_devices
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_set_input_device(ear: edge_ear_h, id: *const c_char) -> i32 {
    with!(ear, e => {
        let id = ok_or_return!(optional_str(id, "the device identifier"));
        report(e.core.set_input_device(id))
    })
}

/// @brief Choose a speaker.
///
/// The name is taken as given and checked when capture starts.
///
/// @param[in] ear the handle
/// @param[in] id an identifier from edge_ear_get_output_devices(), or NULL
///            for the system default
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_get_output_devices
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_set_output_device(ear: edge_ear_h, id: *const c_char) -> i32 {
    with!(ear, e => {
        let id = ok_or_return!(optional_str(id, "the device identifier"));
        report(e.core.set_output_device(id))
    })
}

// ---- formats ------------------------------------------------------

#[repr(i32)]
#[derive(Clone, Copy)]
/// Which of the three readers of live audio a setting is about.
#[allow(non_camel_case_types)]
pub enum edge_ear_target {
    /// The wake word detector. Takes 16 kHz mono 16-bit only.
    EDGE_EAR_TARGET_WAKE = 1,
    /// The speech detector. Takes 16 or 8 kHz mono 16-bit.
    EDGE_EAR_TARGET_SPEECH,
    /// What `edge_ear_read` hands over. Any format the resampler can
    /// produce.
    EDGE_EAR_TARGET_READ,
}

#[repr(i32)]
#[derive(Clone, Copy)]
/// How one sample of audio is written.
#[allow(non_camel_case_types)]
pub enum edge_ear_sample_type {
    /// Signed 16-bit. What `edge_ear_read` requires.
    EDGE_EAR_SAMPLE_TYPE_I16 = 1,
    /// 32-bit float between -1 and 1.
    EDGE_EAR_SAMPLE_TYPE_F32,
}

/// @brief Set the audio format one consumer receives.
///
/// Only before capture starts: once it runs the conversion pipeline is
/// built and changing this would mean rebuilding it.
///
/// @param[in] ear the handle
/// @param[in] target which consumer this is about
/// @param[in] sample_rate in hertz
/// @param[in] channels 1 or 2
/// @param[in] sample_type how one sample is written
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_read
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_set_format(
    ear: edge_ear_h,
    target: edge_ear_target,
    sample_rate: u32,
    channels: u16,
    sample_type: edge_ear_sample_type,
) -> i32 {
    with!(ear, e => {
        let target = match target {
            edge_ear_target::EDGE_EAR_TARGET_WAKE => Target::Wake,
            edge_ear_target::EDGE_EAR_TARGET_SPEECH => Target::Speech,
            edge_ear_target::EDGE_EAR_TARGET_READ => Target::Read,
        };
        let format = AudioFormat::new(sample_rate, channels, sample_type_of(sample_type));
        report(e.core.set_format(target, format))
    })
}

/// @brief How much recent audio each queue keeps.
///
/// This is the ceiling on the pre-roll. Two seconds by default.
///
/// @param[in] ear the handle
/// @param[in] seconds at least as long as the pre-roll
/// @return #EDGE_EAR_OK, or a negative #edge_ear_error.
/// @see edge_ear_set_pre_roll
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_ear_set_ring_capacity(ear: edge_ear_h, seconds: f64) -> i32 {
    with!(ear, e => {
        let span = ok_or_return!(duration(seconds, "the queue capacity"));
        report(e.core.set_ring_capacity(span))
    })
}
