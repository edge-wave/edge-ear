/* Every entry point, called against a real handle: each must link,
 * come back with a code rather than crash, and name a wrong order.
 */

#include <stdio.h>

#include "edge_ear.h"

static int failures = 0;

#define CHECK(cond, what)                                             \
    do {                                                              \
        if (!(cond)) {                                                \
            printf("  FAIL %s: %s\n", (what), edge_ear_get_last_error()); \
            failures++;                                               \
        }                                                             \
    } while (0)

static void on_event(const edge_ear_event *event, void *user)
{
    int *seen = (int *)user;
    if (event->kind == EDGE_EAR_EVENT_SPEECH_ENDED) *seen += 1;
}

static void on_log(edge_ear_log_level level, const char *target,
                   const char *message, void *user)
{
    int *seen = (int *)user;
    if (target != NULL && message != NULL && level != EDGE_EAR_LOG_OFF) *seen += 1;
}

int main(void)
{
    int logged = 0;
    CHECK(edge_ear_set_log_cb(on_log, EDGE_EAR_LOG_DEBUG, &logged) == EDGE_EAR_OK,
          "log callback");

    CHECK(edge_ear_start(NULL) == EDGE_EAR_NULL_ARGUMENT, "null start");
    CHECK(edge_ear_is_running(NULL) == EDGE_EAR_NULL_ARGUMENT, "null is_running");
    edge_ear_free(NULL);

    edge_ear_h ear = edge_ear_new();
    CHECK(ear != NULL, "new");
    if (!ear) return 1;

    int16_t buf[4096];
    size_t got = 0;
    CHECK(edge_ear_read(ear, buf, 4096, 100, &got) == EDGE_EAR_NOT_RUNNING,
          "read before start");
    CHECK(edge_ear_start_recording(ear) == EDGE_EAR_NOT_RUNNING, "record before start");
    CHECK(edge_ear_enable_wake(ear, NULL) == EDGE_EAR_NO_WAKE_MODEL, "wake with no model");
    CHECK(edge_ear_add_wake_model(ear, "w", "nowhere.onnx") == EDGE_EAR_NO_WAKE_MODEL,
          "word before features");
    CHECK(edge_ear_load_wake_features(ear, "nowhere.onnx", "gone.onnx")
              == EDGE_EAR_MODEL_NOT_FOUND, "missing feature models");

    CHECK(edge_ear_add_wake_model(ear, "w", NULL) == EDGE_EAR_NULL_ARGUMENT, "null path");
    /* No name means the file names it, so this gets as far as the
     * models that have not been supplied yet. */
    CHECK(edge_ear_add_wake_model(ear, NULL, "w.onnx") == EDGE_EAR_NO_WAKE_MODEL,
          "a word named after its file");
    CHECK(edge_ear_add_wake_model(ear, "  ", "w.onnx") == EDGE_EAR_INVALID_VALUE,
          "a name of nothing but space");
    CHECK(edge_ear_remove_wake_model(ear, "w") == EDGE_EAR_UNKNOWN_WAKE_WORD,
          "remove unknown word");
    const char *const *words = NULL;
    size_t word_count = 9;
    CHECK(edge_ear_get_wake_models(ear, &words, &word_count) == EDGE_EAR_OK, "list words");
    CHECK(word_count == 0, "no words yet");
    CHECK(edge_ear_play_sound(ear, NULL, 0) == EDGE_EAR_NULL_ARGUMENT, "null sound name");
    CHECK(edge_ear_read(ear, NULL, 10, 0, &got) == EDGE_EAR_NULL_ARGUMENT, "null buffer");
    CHECK(edge_ear_read(ear, buf, 10, 0, NULL) == EDGE_EAR_NULL_ARGUMENT, "null count");

    CHECK(edge_ear_set_wake_threshold(ear, 0.6f) == EDGE_EAR_OK, "wake threshold");
    CHECK(edge_ear_set_wake_threshold(ear, 5.0f) == EDGE_EAR_INVALID_VALUE,
          "threshold out of range");
    CHECK(edge_ear_set_wake_word_threshold(ear, "w", 0.6f) == EDGE_EAR_UNKNOWN_WAKE_WORD,
          "threshold of unknown word");
    CHECK(edge_ear_set_wake_word_threshold(ear, "w", 5.0f) == EDGE_EAR_INVALID_VALUE,
          "word threshold out of range");
    CHECK(edge_ear_unset_wake_word_threshold(ear, "w") == EDGE_EAR_UNKNOWN_WAKE_WORD,
          "unset unknown word");
    float word_threshold = 0.0f;
    CHECK(edge_ear_get_wake_word_threshold(ear, "w", &word_threshold)
              == EDGE_EAR_UNKNOWN_WAKE_WORD, "get unknown word threshold");
    CHECK(edge_ear_set_silence_duration(ear, 0.5) == EDGE_EAR_OK, "silence duration");
    CHECK(edge_ear_set_silence_duration(ear, -1.0) == EDGE_EAR_INVALID_VALUE,
          "negative duration");
    CHECK(edge_ear_set_max_recording(ear, 10.0) == EDGE_EAR_OK, "max recording");
    CHECK(edge_ear_set_no_speech_timeout(ear, 1.0) == EDGE_EAR_OK, "no-speech timeout");
    CHECK(edge_ear_set_pre_roll(ear, 0.2) == EDGE_EAR_OK, "pre-roll");
    CHECK(edge_ear_set_speech_threshold(ear, 0.5f) == EDGE_EAR_OK, "speech threshold");
    CHECK(edge_ear_set_wake_settle_frames(ear, 24) == EDGE_EAR_OK, "settle frames");
    CHECK(edge_ear_set_wake_recording_waits_for_alert(ear, 1) == EDGE_EAR_OK,
          "waiting for the alert");
    CHECK(edge_ear_set_wake_recording_waits_for_alert(ear, 0) == EDGE_EAR_OK,
          "not waiting for the alert");
    CHECK(edge_ear_set_echo_cancellation(ear, 0) == EDGE_EAR_OK, "echo cancellation off");
    CHECK(edge_ear_is_echo_cancellation_enabled(ear) == 0, "no echo cancellation");
    CHECK(edge_ear_set_ring_capacity(ear, 2.0) == EDGE_EAR_OK, "ring capacity");
    CHECK(edge_ear_set_format(ear, EDGE_EAR_TARGET_READ, 16000, 1,
                              EDGE_EAR_SAMPLE_TYPE_I16) == EDGE_EAR_OK, "read format");

    const edge_ear_device *devices;
    size_t count = 0;
    CHECK(edge_ear_get_input_devices(ear, &devices, &count) == EDGE_EAR_OK, "input devices");
    CHECK(count > 0, "at least one microphone");
    /* An empty list is a failure already counted, and reading the first
     * of none would crash before anything could be reported. */
    if (count > 0) {
        CHECK(devices[0].id != NULL && devices[0].name != NULL, "device strings");
    }
    CHECK(edge_ear_get_output_devices(ear, &devices, &count) == EDGE_EAR_OK, "output devices");
    /* A name is taken as given and checked when capture starts, which
     * is what the Rust side does and what its tests pin down. */
    const edge_ear_format *formats;
    size_t formats_count = 0;
    CHECK(edge_ear_get_input_device_formats(ear, NULL, &formats, &formats_count)
              == EDGE_EAR_OK, "input formats");
    CHECK(formats_count > 0, "the default microphone offered something");
    if (formats_count > 0) {
        CHECK(formats[0].channels > 0, "a format names its channels");
        CHECK(formats[0].min_sample_rate <= formats[0].max_sample_rate,
              "a format's span runs the right way");
    }
    CHECK(edge_ear_get_output_device_formats(ear, NULL, &formats, &formats_count)
              == EDGE_EAR_OK, "output formats");
    CHECK(formats_count > 0, "the default speaker offered something");
    CHECK(edge_ear_get_input_device_formats(ear, NULL, NULL, &formats_count)
              == EDGE_EAR_NULL_ARGUMENT, "null formats out");

    edge_ear_format opened;
    CHECK(edge_ear_get_input_format(ear, &opened) == EDGE_EAR_NOT_RUNNING,
          "no microphone format before capture starts");
    CHECK(edge_ear_get_output_format(ear, &opened) == EDGE_EAR_NOT_RUNNING,
          "no speaker format before a sound opens it");
    CHECK(edge_ear_set_input_device_format(ear, 0, 1, EDGE_EAR_SAMPLE_TYPE_I16)
              == EDGE_EAR_OK, "zero rate means the device chooses");
    CHECK(edge_ear_set_input_device_format(ear, 12345, 7, EDGE_EAR_SAMPLE_TYPE_I16)
              == EDGE_EAR_UNSUPPORTED_FORMAT, "a format no device offers");

    CHECK(edge_ear_set_input_device(ear, "nothing::here") == EDGE_EAR_OK,
          "an unknown name is accepted");
    CHECK(edge_ear_start(ear) == EDGE_EAR_NO_DEVICE, "and refused at start");
    CHECK(edge_ear_set_input_device(ear, NULL) == EDGE_EAR_OK, "back to the default");

    int16_t tone[1600];
    for (size_t i = 0; i < 1600; i++) tone[i] = (int16_t)((i % 200) * 30);
    CHECK(edge_ear_register_sound_pcm(ear, "beep", tone, 1600, 16000, 1,
                                      EDGE_EAR_SAMPLE_TYPE_I16, 0.5f)
              == EDGE_EAR_OK, "register pcm");
    CHECK(edge_ear_register_sound_pcm(ear, "bad", NULL, 10, 16000, 1,
                                      EDGE_EAR_SAMPLE_TYPE_I16, 1.0f)
              == EDGE_EAR_NULL_ARGUMENT, "null pcm");
    CHECK(edge_ear_play_sound(ear, "missing", 0) == EDGE_EAR_UNKNOWN_SOUND,
          "unknown sound");
    CHECK(edge_ear_unregister_sound(ear, "beep") == EDGE_EAR_OK, "unregister");

    float score = 0.0f;
    CHECK(edge_ear_get_wake_score(ear, "w", &score) == EDGE_EAR_UNKNOWN_WAKE_WORD,
          "score of unknown word");
    const char *alert = (const char *)1;
    CHECK(edge_ear_get_wake_alert(ear, &alert) == EDGE_EAR_OK, "alert getter");
    CHECK(alert == NULL, "no alert named");
    CHECK(edge_ear_is_wake_enabled(ear) == 0, "wake off");
    CHECK(edge_ear_is_speech_enabled(ear) == 0, "speech off");
    CHECK(edge_ear_is_playing(ear) == 0, "not playing");
    CHECK(edge_ear_is_recording(ear) == 0, "not recording");

    int endings = 0;
    CHECK(edge_ear_set_event_cb(ear, on_event, &endings) == EDGE_EAR_OK, "handler");
    CHECK(edge_ear_start(ear) == EDGE_EAR_OK, "start");
    CHECK(edge_ear_is_running(ear) == 1, "running");
    CHECK(edge_ear_start(ear) == EDGE_EAR_ALREADY_RUNNING, "start twice");
    CHECK(edge_ear_set_format(ear, EDGE_EAR_TARGET_READ, 8000, 1,
                              EDGE_EAR_SAMPLE_TYPE_I16) != EDGE_EAR_OK,
          "format while running");

    CHECK(edge_ear_read(ear, buf, 4096, 3000, &got) == EDGE_EAR_OK, "read");
    CHECK(got > 0, "audio arrived");

    size_t tiny = 0;
    CHECK(edge_ear_read(ear, buf, 1, 3000, &tiny) == EDGE_EAR_INVALID_VALUE,
          "buffer too small");
    CHECK(tiny == 0, "nothing written into a buffer too small");

    CHECK(edge_ear_enable_speech(ear) == EDGE_EAR_OK, "enable speech");
    CHECK(edge_ear_is_speech_enabled(ear) == 1, "speech on");
    CHECK(edge_ear_start_recording(ear) == EDGE_EAR_OK, "start recording");
    CHECK(edge_ear_stop_recording(ear) == EDGE_EAR_OK, "stop recording");
    CHECK(edge_ear_disable_speech(ear) == EDGE_EAR_OK, "disable speech");

    CHECK(edge_ear_stop(ear) == EDGE_EAR_OK, "stop");
    CHECK(edge_ear_is_running(ear) == 0, "stopped");
    edge_ear_free(ear);

    CHECK(logged > 0, "the library logged something");
    CHECK(edge_ear_set_log_cb(NULL, EDGE_EAR_LOG_OFF, NULL) == EDGE_EAR_OK,
          "log callback removed");

    if (failures == 0) {
        printf("every entry point behaved\n");
        return 0;
    }
    printf("%d failed\n", failures);
    return 1;
}
