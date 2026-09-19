/* The shortest useful C program: open the microphone, read, and print
 * how loud it was. Takes a device identifier, or uses the default.
 */

#include <stdio.h>
#include <string.h>

#include "edge_ear.h"

static const char *const level_names[] = {
    "off", "error", "warn", "info", "debug", "trace"
};

static void on_log(edge_ear_log_level level, const char *target,
                   const char *message, void *user)
{
    (void)target;
    (void)user;
    fprintf(stderr, "%s: %s\n", level_names[level], message);
}

static void on_event(const edge_ear_event *event, void *user)
{
    (void)user;
    switch (event->kind) {
    case EDGE_EAR_EVENT_WAKE_DETECTED:
        printf("heard %s (%.2f)\n", event->word, event->score);
        break;
    case EDGE_EAR_EVENT_SPEECH_ENDED:
        printf("recording of %zu samples ended, reason %d\n",
               event->audio_len, (int)event->reason);
        break;
    case EDGE_EAR_EVENT_SOUND_FINISHED:
        printf("sound finished: %s\n", event->sound_id);
        break;
    case EDGE_EAR_EVENT_DEVICE_ERROR:
        printf("device error: %s\n", event->message);
        break;
    default:
        break;
    }
}

int main(int argc, char **argv)
{
    /* Warnings and errors only. Raise it to see the library at work. */
    edge_ear_set_log_cb(on_log, EDGE_EAR_LOG_WARN, NULL);

    edge_ear_h ear = edge_ear_new();
    if (!ear) {
        fprintf(stderr, "could not open a device: %s\n", edge_ear_get_last_error());
        return 1;
    }

    const edge_ear_device *devices;
    size_t count;
    if (edge_ear_get_input_devices(ear, &devices, &count) == EDGE_EAR_OK) {
        printf("%zu microphones\n", count);
        for (size_t i = 0; i < count; i++) {
            printf("  %s%s\n", devices[i].name, devices[i].is_default ? " (default)" : "");
        }
    }

    if (argc > 1 && edge_ear_set_input_device(ear, argv[1]) != EDGE_EAR_OK) {
        fprintf(stderr, "no such microphone: %s\n", edge_ear_get_last_error());
        edge_ear_free(ear);
        return 1;
    }

    edge_ear_set_event_cb(ear, on_event, NULL);

    int rc = edge_ear_start(ear);
    if (rc != EDGE_EAR_OK) {
        fprintf(stderr, "could not start: %s\n", edge_ear_get_last_error());
        edge_ear_free(ear);
        return 1;
    }

    int16_t buf[16000];
    for (int block = 0; block < 20; block++) {
        size_t got = 0;
        rc = edge_ear_read(ear, buf, sizeof buf / sizeof buf[0], 2000, &got);
        if (rc != EDGE_EAR_OK) {
            fprintf(stderr, "read failed: %s\n", edge_ear_get_last_error());
            break;
        }
        long peak = 0;
        for (size_t i = 0; i < got; i++) {
            long v = buf[i] < 0 ? -(long)buf[i] : buf[i];
            if (v > peak) peak = v;
        }
        printf("%3d: %zu samples, peak %ld\n", block, got, peak);
    }

    edge_ear_stop(ear);
    edge_ear_free(ear);
    return 0;
}
