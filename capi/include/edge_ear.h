/**
 * @file
 * @brief edge-ear: a local audio front end.
 *
 * Generated from the Rust source by cbindgen; do not
 * edit. Regenerate with:
 *     cbindgen --config capi/cbindgen.toml --crate edge-ear-capi *         --output capi/include/edge_ear.h
 *
 * Errors: every fallible call returns int. 0 is success, negative names
 * the failure, and edge_ear_get_last_error() describes the most recent one
 * on this thread.
 *
 * Memory: the caller owns everything it passes in, and the library
 * hands out nothing the caller must free. Strings out are borrowed for
 * the length of the call unless said otherwise.
 *
 * const: handles are never const. Reading one can still write to it,
 * because anything handed back as a borrowed string is kept there, and
 * a promise the compiler cannot check is worse than none.
 *
 * Threads: a handle is usable from several threads. edge_ear_free is
 * the exception, and must not run alongside another call on the same
 * handle or from inside a handler.
 */


#ifndef EDGE_EAR_H
#define EDGE_EAR_H

#include <stddef.h>
#include <stdint.h>

/**
 * Which notification arrived.
 */
enum edge_ear_event_kind
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : int32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /**
     * The wake word was heard.
     */
    EDGE_EAR_EVENT_WAKE_DETECTED = 1,
    /**
     * A recording ended and the audio is here.
     */
    EDGE_EAR_EVENT_SPEECH_ENDED,
    /**
     * A sound reached its own end. One that was stopped does not
     * arrive here.
     */
    EDGE_EAR_EVENT_SOUND_FINISHED,
    /**
     * A device failed. Capture has stopped.
     */
    EDGE_EAR_EVENT_DEVICE_ERROR,
    /**
     * Notifications were thrown away because a handler could not
     * keep up.
     */
    EDGE_EAR_EVENT_EVENTS_DROPPED,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum edge_ear_event_kind edge_ear_event_kind;
#else
typedef int32_t edge_ear_event_kind;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/**
 * What brought a recording to an end.
 */
enum edge_ear_end_reason
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : int32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /**
     * No recording ended; this notification is something else.
     */
    EDGE_EAR_END_NONE = 0,
    /**
     * The speaker went quiet for the silence duration.
     */
    EDGE_EAR_END_SILENCE,
    /**
     * The length cap was reached while someone was still talking.
     */
    EDGE_EAR_END_MAX_LENGTH,
    /**
     * Nobody spoke at all before the timeout.
     */
    EDGE_EAR_END_NO_SPEECH,
    /**
     * The application ended it.
     */
    EDGE_EAR_END_STOPPED,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum edge_ear_end_reason edge_ear_end_reason;
#else
typedef int32_t edge_ear_end_reason;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/**
 * How one sample of audio is written.
 */
enum edge_ear_sample_type
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : int32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /**
     * Signed 16-bit. What `edge_ear_read` requires.
     */
    EDGE_EAR_SAMPLE_TYPE_I16 = 1,
    /**
     * 32-bit float between -1 and 1.
     */
    EDGE_EAR_SAMPLE_TYPE_F32,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum edge_ear_sample_type edge_ear_sample_type;
#else
typedef int32_t edge_ear_sample_type;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/**
 * Which of the three readers of live audio a setting is about.
 */
enum edge_ear_target
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : int32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /**
     * The wake word detector. Takes 16 kHz mono 16-bit only.
     */
    EDGE_EAR_TARGET_WAKE = 1,
    /**
     * The speech detector. Takes 16 or 8 kHz mono 16-bit.
     */
    EDGE_EAR_TARGET_SPEECH,
    /**
     * What `edge_ear_read` hands over. Any format the resampler can
     * produce.
     */
    EDGE_EAR_TARGET_READ,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum edge_ear_target edge_ear_target;
#else
typedef int32_t edge_ear_target;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/**
 * What went wrong. Zero is success; everything else is negative, one
 * value for each failure the core reports.
 */
enum edge_ear_error
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : int32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    /**
     * It worked.
     */
    EDGE_EAR_OK = 0,
    /**
     * Capture is not running.
     */
    EDGE_EAR_NOT_RUNNING = -1,
    /**
     * Capture is already running.
     */
    EDGE_EAR_ALREADY_RUNNING = -2,
    /**
     * Can only be set before capture starts.
     */
    EDGE_EAR_RUNNING_NOT_ALLOWED = -3,
    /**
     * Refused while a recording is open, because it took this value
     * as it opened and a new one cannot reach it.
     */
    EDGE_EAR_RECORDING_OPEN = -4,
    /**
     * The handle has been freed.
     */
    EDGE_EAR_DESTROYED = -5,
    /**
     * No wake word model has been loaded.
     */
    EDGE_EAR_NO_WAKE_MODEL = -6,
    /**
     * No model file at that path.
     */
    EDGE_EAR_MODEL_NOT_FOUND = -7,
    /**
     * The model file could not be read.
     */
    EDGE_EAR_MODEL_UNREADABLE = -8,
    /**
     * The file is not a model this pipeline can use.
     */
    EDGE_EAR_MODEL_INVALID = -9,
    /**
     * That part of the library does not take this audio format.
     */
    EDGE_EAR_UNSUPPORTED_FORMAT = -10,
    /**
     * The value is outside what the setting allows.
     */
    EDGE_EAR_INVALID_VALUE = -11,
    /**
     * No such device, or none available at all.
     */
    EDGE_EAR_NO_DEVICE = -12,
    /**
     * The system refused access to the microphone.
     */
    EDGE_EAR_PERMISSION_DENIED = -13,
    /**
     * The device went away while it was in use.
     */
    EDGE_EAR_DEVICE_LOST = -14,
    /**
     * No sound is registered under that name.
     */
    EDGE_EAR_UNKNOWN_SOUND = -15,
    /**
     * Nothing arrived before the time given ran out.
     */
    EDGE_EAR_TIMEOUT = -16,
    /**
     * Capture stopped while this call was waiting.
     */
    EDGE_EAR_STOPPED = -17,
    /**
     * The device itself failed. The message says how.
     */
    EDGE_EAR_BACKEND = -18,
    /**
     * The audio could not be converted to the wanted format.
     */
    EDGE_EAR_CONVERSION = -19,
    /**
     * Something required was null.
     */
    EDGE_EAR_NULL_ARGUMENT = -20,
    /**
     * A string was not valid UTF-8. It is refused rather than
     * replaced or cut short.
     */
    EDGE_EAR_NOT_UTF8 = -21,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum edge_ear_error edge_ear_error;
#else
typedef int32_t edge_ear_error;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

/**
 * What a handle points to. Opaque on the C side, which only ever
 * names the pointer to this: `edge_ear_h`.
 */
typedef struct edge_ear_handle edge_ear_handle;

/**
 * The handle a C caller holds.
 */
typedef edge_ear_handle *edge_ear_h;

/**
 * One notification, as C sees it. Every pointer borrows from the call
 * that raised it and stops being valid when the handler returns.
 */
typedef struct {
    /**
     * Which notification this is. It decides which fields below mean
     * anything; the rest are zero or null.
     */
    edge_ear_event_kind kind;
    /**
     * Wake detected: how sure the detector was, from 0.0 to 1.0.
     */
    float score;
    /**
     * Speech ended: the recording, including any pre-roll.
     */
    const int16_t *audio;
    /**
     * Speech ended: how many samples `audio` holds.
     */
    uintptr_t audio_len;
    /**
     * Speech ended: the rate those samples were taken at.
     */
    uint32_t sample_rate;
    /**
     * Speech ended: what brought the recording to an end.
     */
    edge_ear_end_reason reason;
    /**
     * Speech ended: audio counted after the recording opened, which
     * leaves out any pre-roll in front of it.
     */
    double duration_secs;
    /**
     * Sound finished: the name it was registered under.
     */
    const char *sound_id;
    /**
     * Device error: what went wrong, in plain words.
     */
    const char *message;
    /**
     * Events dropped: how many the application was never told about,
     * because a handler could not keep up.
     */
    uint64_t dropped;
} edge_ear_event;

/**
 * Called for every notification. The event stops being valid when it
 * returns, so anything kept must be copied first.
 */
typedef void (*edge_ear_event_cb)(const edge_ear_event *event, void *user);

/**
 * One shape of audio a device says it will take.
 */
typedef struct {
    /**
     * How many channels at this setting.
     */
    uint16_t channels;
    /**
     * The lowest rate it will take here.
     */
    uint32_t min_sample_rate;
    /**
     * The highest rate it will take here. Equal to the lowest when a
     * device offers one rate rather than a span.
     */
    uint32_t max_sample_rate;
    /**
     * What this library hands over, or takes, at this setting.
     */
    edge_ear_sample_type sample_type;
} edge_ear_format;

/**
 * One device. Both strings are borrowed until the next listing call
 * on the same handle.
 */
typedef struct {
    /**
     * What to pass to `edge_ear_set_input_device`. Stable across runs.
     */
    const char *id;
    /**
     * What to show a person. Two devices may share one.
     */
    const char *name;
    /**
     * One when the system would pick this without being asked.
     */
    int32_t is_default;
} edge_ear_device;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * @brief The message behind the last failing call on this thread.
 *
 * @return The message, borrowed until the next call on this thread
 *         fails. Empty when nothing has failed yet.
 */
const char *edge_ear_get_last_error(void);

/**
 * @brief Make a handle.
 *
 * @return The handle, or NULL when no device could be reached. On
 *         NULL, edge_ear_get_last_error() says why.
 * @see edge_ear_free
 */
edge_ear_h edge_ear_new(void);

/**
 * @brief Release the handle and everything it owns.
 *
 * Must not run alongside another call on the same handle, or from
 * inside a handler.
 *
 * @param[in] ear the handle, or NULL to do nothing
 * @see edge_ear_new
 */
void edge_ear_free(edge_ear_h ear);

/**
 * @brief Open the microphone and begin reading.
 *
 * Formats and devices are fixed from here until the handle is stopped.
 *
 * @param[in] ear the handle
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_stop, edge_ear_read
 */
int32_t edge_ear_start(edge_ear_h ear);

/**
 * @brief Release the microphone and the speaker.
 *
 * The handle can be set up and started again afterwards.
 *
 * @param[in] ear the handle
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_start
 */
int32_t edge_ear_stop(edge_ear_h ear);

/**
 * @brief Whether capture is running.
 *
 * @param[in] ear the handle
 * @return 1 while running, 0 when not, #EDGE_EAR_NULL_ARGUMENT for a
 *         null handle.
 * @see edge_ear_start
 */
int32_t edge_ear_is_running(edge_ear_h ear);

/**
 * @brief Take the next block of live audio.
 *
 * A block larger than `cap` is refused rather than truncated, so
 * nothing is ever lost without being told.
 *
 * @param[in] ear the handle
 * @param[out] buf where the samples go
 * @param[in]  cap how many samples fit in `buf`
 * @param[in]  timeout_ms how long to wait; below zero waits until
 *             audio arrives or capture stops
 * @param[out] got how many samples were written
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_set_format
 */
int32_t edge_ear_read(edge_ear_h ear,
                      int16_t *buf,
                      uintptr_t cap,
                      int32_t timeout_ms,
                      uintptr_t *got);

/**
 * @brief Set the handler called for every notification.
 *
 * It runs on the one thread that delivers notifications, so a slow
 * handler holds up later notifications only. It may call back in.
 *
 * @param[in] ear the handle
 * @param[in] callback the handler, or NULL to remove the current one
 * @param[in] user passed to the handler untouched; the caller keeps it
 *            alive until the handle is freed
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 */
int32_t edge_ear_on_event(edge_ear_h ear, edge_ear_event_cb callback, void *user);

/**
 * @brief Supply the two models every wake word shares.
 *
 * Neither knows any word, and neither ships with this library.
 *
 * @param[in] ear the handle
 * @param[in] spectrogram path to the melspectrogram model
 * @param[in] features path to the speech embedding model
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_load_wake_model
 */
int32_t edge_ear_load_wake_features(edge_ear_h ear, const char *spectrogram, const char *features);

/**
 * @brief Supply the model for the phrase to listen for.
 *
 * Its shape is checked here, and a model built for another pipeline is
 * refused by name of what was wrong.
 *
 * @param[in] ear the handle
 * @param[in] path path to the wake word model
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_load_wake_features, edge_ear_enable_wake
 */
int32_t edge_ear_load_wake_model(edge_ear_h ear, const char *path);

/**
 * @brief Start listening for the wake word.
 *
 * @param[in] ear the handle
 * @param[in] alert a registered sound to play on detection, or NULL.
 *            Naming one also holds off counting silence until it ends,
 *            so its tail is not taken for speech
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_disable_wake, edge_ear_register_sound_file
 */
int32_t edge_ear_enable_wake(edge_ear_h ear, const char *alert);

/**
 * @brief Stop listening for the wake word.
 *
 * The models stay loaded.
 *
 * @param[in] ear the handle
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_enable_wake
 */
int32_t edge_ear_disable_wake(edge_ear_h ear);

/**
 * @brief Whether the wake word is being listened for.
 *
 * @param[in] ear the handle
 * @return 1 when listening, 0 when not, #EDGE_EAR_NULL_ARGUMENT for a
 *         null handle.
 */
int32_t edge_ear_is_wake_enabled(edge_ear_h ear);

/**
 * @brief Clear what the detector has heard and look away.
 *
 * The same as if the word had just been heard.
 *
 * @param[in] ear the handle
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_set_wake_settle_frames
 */
int32_t edge_ear_reset_wake(edge_ear_h ear);

/**
 * @brief How sure the detector was, most recently.
 *
 * Every score, not only the ones that counted, because choosing a
 * threshold is guesswork without seeing the near misses.
 *
 * @param[in] ear the handle
 * @param[out] score where the score goes, from 0.0 to 1.0
 * @return #EDGE_EAR_OK, or #EDGE_EAR_NOT_RUNNING when nothing has
 *         scored yet.
 * @see edge_ear_set_wake_threshold
 */
int32_t edge_ear_get_wake_score(edge_ear_h ear, float *score);

/**
 * @brief The sound played on detection.
 *
 * @param[in] ear the handle
 * @param[out] alert where the name goes, or NULL when none was named.
 *             Borrowed until the next call replaces it
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_enable_wake
 */
int32_t edge_ear_get_wake_alert(edge_ear_h ear, const char **alert);

/**
 * @brief How sure the detector must be before it says it heard.
 *
 * @param[in] ear the handle
 * @param[in] value from 0.0 to 1.0
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_get_wake_score
 */
int32_t edge_ear_set_wake_threshold(edge_ear_h ear, float value);

/**
 * @brief How long to look away after hearing the word.
 *
 * Long enough that the same words are not heard twice on their way out
 * of the pipeline.
 *
 * @param[in] ear the handle
 * @param[in] frames how many frames of 80 ms to ignore
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_reset_wake
 */
int32_t edge_ear_set_wake_settle_frames(edge_ear_h ear, uint32_t frames);

/**
 * @brief Start watching for the end of speech.
 *
 * Takes effect on the next block of audio.
 *
 * @param[in] ear the handle
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_disable_speech, edge_ear_start_recording
 */
int32_t edge_ear_enable_speech(edge_ear_h ear);

/**
 * @brief Stop watching for speech.
 *
 * Any open recording is dropped.
 *
 * @param[in] ear the handle
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_enable_speech
 */
int32_t edge_ear_disable_speech(edge_ear_h ear);

/**
 * @brief Whether speech and silence are being watched for.
 *
 * @param[in] ear the handle
 * @return 1 when watching, 0 when not, #EDGE_EAR_NULL_ARGUMENT for a
 *         null handle.
 */
int32_t edge_ear_is_speech_enabled(edge_ear_h ear);

/**
 * @brief Open a recording.
 *
 * It ends on silence, on the no-speech timeout, at the length cap, or
 * when stopped, and the audio arrives as a notification.
 *
 * @param[in] ear the handle
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_stop_recording, edge_ear_set_pre_roll
 */
int32_t edge_ear_start_recording(edge_ear_h ear);

/**
 * @brief End the open recording.
 *
 * One notification follows, saying it was stopped rather than that the
 * speaker went quiet.
 *
 * @param[in] ear the handle
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_start_recording
 */
int32_t edge_ear_stop_recording(edge_ear_h ear);

/**
 * @brief Whether a recording is collecting audio.
 *
 * @param[in] ear the handle
 * @return 1 while recording, 0 when not, #EDGE_EAR_NULL_ARGUMENT for a
 *         null handle.
 */
int32_t edge_ear_is_recording(edge_ear_h ear);

/**
 * @brief How readily audio counts as speech.
 *
 * @param[in] ear the handle
 * @param[in] value from 0.0 to 1.0
 * @return #EDGE_EAR_OK or a negative #edge_ear_error. Taken on the
 *         next frame, so an open recording follows the new value.
 */
int32_t edge_ear_set_speech_threshold(edge_ear_h ear, float value);

/**
 * @brief How long the speaker must be quiet before a recording ends.
 *
 * @param[in] ear the handle
 * @param[in] seconds greater than zero
 * @return #EDGE_EAR_OK or a negative #edge_ear_error. Taken on the
 *         next frame, so an open recording follows the new value.
 */
int32_t edge_ear_set_silence_duration(edge_ear_h ear, double seconds);

/**
 * @brief The longest a recording may run before it is handed over.
 *
 * @param[in] ear the handle
 * @param[in] seconds greater than the silence duration
 * @return #EDGE_EAR_OK or a negative #edge_ear_error. Taken on the
 *         next frame, so an open recording follows the new value.
 * @see edge_ear_set_silence_duration
 */
int32_t edge_ear_set_max_recording(edge_ear_h ear, double seconds);

/**
 * @brief How long to wait for anyone to speak at all.
 *
 * @param[in] ear the handle
 * @param[in] seconds greater than zero
 * @return #EDGE_EAR_OK or a negative #edge_ear_error. Taken on the
 *         next frame, so an open recording follows the new value.
 */
int32_t edge_ear_set_no_speech_timeout(edge_ear_h ear, double seconds);

/**
 * @brief How much audio from before the recording to include.
 *
 * So a word begun early is not cut off. Used by a recording the wake
 * word opened as much as by one this application asked for.
 *
 * @param[in] ear the handle
 * @param[in] seconds no more than the queue capacity
 * @return #EDGE_EAR_OK, #EDGE_EAR_RECORDING_OPEN while a recording is
 *         open, or another negative #edge_ear_error.
 * @see edge_ear_set_ring_capacity,
 *      edge_ear_set_wake_recording_waits_for_alert
 */
int32_t edge_ear_set_pre_roll(edge_ear_h ear, double seconds);

/**
 * @brief Whether a wake word recording begins again when the alert
 *        ends.
 *
 * Off by default, so the recording collects from the moment the wake
 * word lands and the microphone hears the alert into it. Turning it
 * on leaves the alert behind, at the price of a speaker who talks
 * over it, and puts the pre-roll out of reach on that path.
 *
 * @param[in] ear the handle
 * @param[in] waits non-zero to wait for the alert
 * @return #EDGE_EAR_OK, #EDGE_EAR_RECORDING_OPEN while a recording is
 *         open, or another negative #edge_ear_error.
 * @see edge_ear_enable_wake, edge_ear_set_pre_roll
 */
int32_t edge_ear_set_wake_recording_waits_for_alert(edge_ear_h ear, int32_t waits);

/**
 * @brief Register a sound read from a file.
 *
 * Decoding happens now, so playing it later is only a copy. Allowed
 * while running.
 *
 * @param[in] ear the handle
 * @param[in] id the name to play it by later
 * @param[in] path a wav or ogg file
 * @param[in] volume from 0.0 to 1.0
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_play_sound, edge_ear_unregister_sound
 */
int32_t edge_ear_register_sound_file(edge_ear_h ear,
                                     const char *id,
                                     const char *path,
                                     float volume);

/**
 * @brief Register a sound from raw audio the caller already holds.
 *
 * Raw audio carries no header, so `sample_rate`, `channels` and
 * `sample_type` say what `data` holds. The samples are copied, so they
 * may be freed once this returns.
 *
 * @param[in] ear the handle
 * @param[in] id the name to play it by later
 * @param[in] data samples in the type named below
 * @param[in] len how many samples `data` holds, not bytes
 * @param[in] sample_rate the rate those samples were taken at
 * @param[in] channels 1 or 2
 * @param[in] sample_type how one sample is written
 * @param[in] volume from 0.0 to 1.0
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_play_sound, edge_ear_unregister_sound
 */
int32_t edge_ear_register_sound_pcm(edge_ear_h ear,
                                    const char *id,
                                    const void *data,
                                    uintptr_t len,
                                    uint32_t sample_rate,
                                    uint16_t channels,
                                    edge_ear_sample_type sample_type,
                                    float volume);

/**
 * @brief Forget a registered sound.
 *
 * One already playing is left to finish.
 *
 * @param[in] ear the handle
 * @param[in] id the name it was registered under
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_register_sound_file
 */
int32_t edge_ear_unregister_sound(edge_ear_h ear, const char *id);

/**
 * @brief Play a registered sound.
 *
 * @param[in] ear the handle
 * @param[in] id the name it was registered under
 * @param[in] repeat non-zero loops it until stopped
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_stop_sound, edge_ear_is_playing
 */
int32_t edge_ear_play_sound(edge_ear_h ear, const char *id, int32_t repeat);

/**
 * @brief Stop whatever is playing.
 *
 * No finished notification follows, because the sound did not reach
 * its own end.
 *
 * @param[in] ear the handle
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_play_sound
 */
int32_t edge_ear_stop_sound(edge_ear_h ear);

/**
 * @brief Whether a sound is coming out of the speaker.
 *
 * @param[in] ear the handle
 * @return 1 while playing, 0 when not, #EDGE_EAR_NULL_ARGUMENT for a
 *         null handle.
 */
int32_t edge_ear_is_playing(edge_ear_h ear);

/**
 * @brief Open the microphone at this rather than at its default.
 *
 * Refused here if the named device does not offer it, and again when
 * capture starts, because the device may have changed by then. Pass
 * zero for `sample_rate` to go back to the device's own choice.
 *
 * @param[in] ear the handle
 * @param[in] sample_rate the rate to open at, or 0 for the default
 * @param[in] channels 1 or 2
 * @param[in] sample_type how one sample is written
 * @return #EDGE_EAR_OK, #EDGE_EAR_UNSUPPORTED_FORMAT when the device
 *         does not offer it, or another negative #edge_ear_error.
 * @see edge_ear_get_input_device_formats, edge_ear_get_input_format
 */
int32_t edge_ear_set_input_device_format(edge_ear_h ear,
                                         uint32_t sample_rate,
                                         uint16_t channels,
                                         edge_ear_sample_type sample_type);

/**
 * @brief Open the speaker at this rather than at its default.
 *
 * Fixed once the speaker is open, which is when the first sound is
 * registered or played. Pass zero for `sample_rate` to go back to the
 * device's own choice.
 *
 * @param[in] ear the handle
 * @param[in] sample_rate the rate to open at, or 0 for the default
 * @param[in] channels 1 or 2
 * @param[in] sample_type how one sample is written
 * @return #EDGE_EAR_OK, #EDGE_EAR_UNSUPPORTED_FORMAT when the device
 *         does not offer it, #EDGE_EAR_RUNNING_NOT_ALLOWED once the
 *         speaker is open, or another negative #edge_ear_error.
 * @see edge_ear_get_output_device_formats, edge_ear_get_output_format
 */
int32_t edge_ear_set_output_device_format(edge_ear_h ear,
                                          uint32_t sample_rate,
                                          uint16_t channels,
                                          edge_ear_sample_type sample_type);

/**
 * @brief What the microphone opened at.
 *
 * Not always what was asked for. The two rates in `format` are equal,
 * because an open device runs at one.
 *
 * @param[in] ear the handle
 * @param[out] format where it goes
 * @return #EDGE_EAR_OK, #EDGE_EAR_NOT_RUNNING before capture starts,
 *         or another negative #edge_ear_error.
 * @see edge_ear_set_input_device_format
 */
int32_t edge_ear_get_input_format(edge_ear_h ear, edge_ear_format *format);

/**
 * @brief What the speaker opened at.
 *
 * Not always what was asked for. The two rates in `format` are equal,
 * because an open device runs at one.
 *
 * @param[in] ear the handle
 * @param[out] format where it goes
 * @return #EDGE_EAR_OK, #EDGE_EAR_NOT_RUNNING before the first sound
 *         opens the speaker, or another negative #edge_ear_error.
 * @see edge_ear_set_output_device_format
 */
int32_t edge_ear_get_output_format(edge_ear_h ear, edge_ear_format *format);

/**
 * @brief What one microphone will take.
 *
 * Rates come as a span, because that is how a device describes
 * itself. A device offering single rates reports each one with the
 * same low and high.
 *
 * @param[in] ear the handle
 * @param[in] device the identifier, or NULL for the default one
 * @param[out] formats where the list goes, borrowed until the next
 *             listing call on this handle
 * @param[out] count how many entries the list holds
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_get_input_devices, edge_ear_set_format
 */
int32_t edge_ear_get_input_device_formats(edge_ear_h ear,
                                          const char *device,
                                          const edge_ear_format **formats,
                                          uintptr_t *count);

/**
 * @brief What one speaker will take.
 *
 * Sounds are converted to whichever of these the speaker is opened
 * at, so this says what to expect of them.
 *
 * @param[in] ear the handle
 * @param[in] device the identifier, or NULL for the default one
 * @param[out] formats where the list goes, borrowed until the next
 *             listing call on this handle
 * @param[out] count how many entries the list holds
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_get_output_devices, edge_ear_register_sound_pcm
 */
int32_t edge_ear_get_output_device_formats(edge_ear_h ear,
                                           const char *device,
                                           const edge_ear_format **formats,
                                           uintptr_t *count);

/**
 * @brief Every microphone the system offers.
 *
 * @param[in] ear the handle
 * @param[out] devices where the list goes, borrowed until the next
 *             listing call on this handle
 * @param[out] count how many devices the list holds
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_set_input_device
 */
int32_t edge_ear_get_input_devices(edge_ear_h ear,
                                   const edge_ear_device **devices,
                                   uintptr_t *count);

/**
 * @brief Every speaker the system offers.
 *
 * @param[in] ear the handle
 * @param[out] devices where the list goes, borrowed until the next
 *             listing call on this handle
 * @param[out] count how many devices the list holds
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_set_output_device
 */
int32_t edge_ear_get_output_devices(edge_ear_h ear,
                                    const edge_ear_device **devices,
                                    uintptr_t *count);

/**
 * @brief Choose a microphone.
 *
 * The name is taken as given and checked when capture starts.
 *
 * @param[in] ear the handle
 * @param[in] id an identifier from edge_ear_get_input_devices(), or NULL
 *            for the system default
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_get_input_devices
 */
int32_t edge_ear_set_input_device(edge_ear_h ear, const char *id);

/**
 * @brief Choose a speaker.
 *
 * The name is taken as given and checked when capture starts.
 *
 * @param[in] ear the handle
 * @param[in] id an identifier from edge_ear_get_output_devices(), or NULL
 *            for the system default
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_get_output_devices
 */
int32_t edge_ear_set_output_device(edge_ear_h ear, const char *id);

/**
 * @brief Set the audio format one consumer receives.
 *
 * Only before capture starts: once it runs the conversion pipeline is
 * built and changing this would mean rebuilding it.
 *
 * @param[in] ear the handle
 * @param[in] target which consumer this is about
 * @param[in] sample_rate in hertz
 * @param[in] channels 1 or 2
 * @param[in] sample_type how one sample is written
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_read
 */
int32_t edge_ear_set_format(edge_ear_h ear,
                            edge_ear_target target,
                            uint32_t sample_rate,
                            uint16_t channels,
                            edge_ear_sample_type sample_type);

/**
 * @brief How much recent audio each queue keeps.
 *
 * This is the ceiling on the pre-roll.
 *
 * @param[in] ear the handle
 * @param[in] seconds at least as long as the pre-roll
 * @return #EDGE_EAR_OK, or a negative #edge_ear_error.
 * @see edge_ear_set_pre_roll
 */
int32_t edge_ear_set_ring_capacity(edge_ear_h ear, double seconds);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* EDGE_EAR_H */
