"""Every way a call can fail arrives as its own exception class.

An application must be able to catch a refused microphone separately
from a missing one, so one exception type for everything is not enough.
"""

import pytest

import edge_ear


def test_every_error_is_its_own_class():
    names = [
        "NotRunning", "AlreadyRunning", "RunningNotAllowed", "RecordingOpen",
        "Destroyed", "NoWakeModel", "ModelNotFound", "ModelUnreadable",
        "ModelInvalid", "UnsupportedFormat", "InvalidValue", "NoDevice",
        "PermissionDenied", "DeviceLost", "UnknownSound", "UnknownWakeWord",
        "Timeout",
        "Stopped", "BackendError", "ConversionError",
    ]
    seen = set()
    for name in names:
        cls = getattr(edge_ear, name)
        assert issubclass(cls, edge_ear.EdgeEarError), name
        assert cls not in seen, f"{name} is not its own class"
        seen.add(cls)


def test_catching_the_base_class_catches_them_all():
    ear = edge_ear.EdgeEar()
    with pytest.raises(edge_ear.EdgeEarError):
        ear.stop()


def test_stopping_before_starting_says_capture_is_not_running():
    ear = edge_ear.EdgeEar()
    with pytest.raises(edge_ear.NotRunning):
        ear.stop()


def test_starting_twice_says_it_is_already_running():
    ear = edge_ear.EdgeEar()
    ear.start()
    try:
        with pytest.raises(edge_ear.AlreadyRunning):
            ear.start()
    finally:
        ear.close()


def test_reading_before_starting_says_capture_is_not_running():
    ear = edge_ear.EdgeEar()
    with pytest.raises(edge_ear.NotRunning):
        ear.read()


def test_every_call_after_close_says_the_handle_is_gone():
    ear = edge_ear.EdgeEar()
    ear.close()
    for call in (ear.start, ear.stop, ear.read, ear.enable_speech):
        with pytest.raises(edge_ear.Destroyed):
            call()


def test_playing_a_sound_nobody_registered_says_so():
    with edge_ear.EdgeEar() as ear:
        with pytest.raises(edge_ear.UnknownSound):
            ear.play_sound("never-registered")


def test_a_name_c_could_not_carry_is_refused():
    # The name reaches C as text, where a NUL would cut it short.
    with edge_ear.EdgeEar() as ear:
        for name in ("", " ", "a\0b", "two\nlines"):
            with pytest.raises(edge_ear.InvalidValue):
                ear.add_wake_model("anything.onnx", name)
        assert ear.wake_models == []


def test_a_wake_word_nobody_added_says_so():
    with edge_ear.EdgeEar() as ear:
        assert ear.wake_models == []
        for call in (
            lambda: ear.remove_wake_model("jarvis"),
            lambda: ear.wake_score("jarvis"),
            lambda: ear.set_wake_word_threshold("jarvis", 0.6),
            lambda: ear.wake_word_threshold("jarvis"),
        ):
            with pytest.raises(edge_ear.UnknownWakeWord):
                call()


def test_a_volume_outside_the_range_is_refused():
    with edge_ear.EdgeEar() as ear:
        with pytest.raises(edge_ear.InvalidValue):
            ear.register_sound("alert", pcm=[0] * 1600, volume=5.0)


def test_a_format_the_model_cannot_take_is_refused():
    with edge_ear.EdgeEar() as ear:
        with pytest.raises(edge_ear.UnsupportedFormat):
            ear.set_format("wake", sample_rate=48000)


def test_a_format_change_after_start_is_refused():
    with edge_ear.EdgeEar() as ear:
        ear.start()
        with pytest.raises(edge_ear.RunningNotAllowed):
            ear.set_format("read", sample_rate=8000)


def test_giving_neither_a_path_nor_audio_is_refused():
    with edge_ear.EdgeEar() as ear:
        with pytest.raises(edge_ear.InvalidValue):
            ear.register_sound("nothing")
        with pytest.raises(edge_ear.InvalidValue):
            ear.register_sound("both", path="a.wav", pcm=[0])


def test_a_missing_sound_file_is_reported_as_missing():
    with edge_ear.EdgeEar() as ear:
        with pytest.raises(edge_ear.ModelNotFound):
            ear.register_sound("alert", path="/no/such/sound.wav")
