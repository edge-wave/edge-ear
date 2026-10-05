"""Getting hold of the devices, and letting go of them again."""

import struct
import subprocess
import sys

import pytest

import edge_ear


def test_the_with_block_releases_the_devices():
    with edge_ear.EdgeEar() as ear:
        ear.start()
        assert ear.is_running

    # Outside the block the handle is finished.
    with pytest.raises(edge_ear.Destroyed):
        ear.start()


def test_the_with_block_releases_them_even_when_it_raises():
    ear_outside = None
    with pytest.raises(ValueError):
        with edge_ear.EdgeEar() as ear:
            ear_outside = ear
            ear.start()
            raise ValueError("something went wrong in here")

    with pytest.raises(edge_ear.Destroyed):
        ear_outside.start()


def test_a_handle_left_open_at_exit_goes_quietly():
    # Shutdown destroys the handle after the interpreter is finalized,
    # and what core logs then must not try to reach Python.
    script = "import edge_ear\near = edge_ear.EdgeEar()\n"
    done = subprocess.run(
        [sys.executable, "-c", script], capture_output=True, text=True, timeout=60
    )
    assert done.returncode == 0, done.stderr
    assert "panicked" not in done.stderr, done.stderr


def test_start_and_stop_can_repeat():
    with edge_ear.EdgeEar() as ear:
        for _ in range(3):
            ear.start()
            assert ear.is_running
            ear.stop()
            assert not ear.is_running


def test_closing_twice_is_harmless():
    ear = edge_ear.EdgeEar()
    ear.close()
    ear.close()


def test_devices_carry_an_identifier_a_name_and_one_default():
    with edge_ear.EdgeEar() as ear:
        devices = ear.input_devices()
        assert devices, "expected at least one microphone"
        assert len({d.id for d in devices}) == len(devices), "identifiers repeat"
        assert sum(1 for d in devices if d.is_default) == 1
        for d in devices:
            assert d.id and d.name


def test_audio_comes_back_as_bytes():
    with edge_ear.EdgeEar() as ear:
        ear.start()
        chunk = ear.read(timeout=2.0)
        assert isinstance(chunk.audio, bytes)
        assert len(chunk.audio) == len(chunk) * 2, "16 bits a sample"
        assert chunk.sample_rate == 16000
        assert chunk.channels == 1
        assert chunk.dropped_before >= 0

        # The bytes really are little-endian 16-bit samples.
        count = min(4, len(chunk))
        struct.unpack(f"<{count}h", chunk.audio[: count * 2])


def test_audio_comes_back_as_a_numpy_array_when_numpy_is_there():
    numpy = pytest.importorskip("numpy")
    with edge_ear.EdgeEar() as ear:
        ear.start()
        chunk = ear.read(timeout=2.0)
        array = chunk.numpy()
        assert array.dtype == numpy.int16
        assert len(array) == len(chunk)


def test_waiting_for_the_alert_is_off_until_it_is_asked_for():
    with edge_ear.EdgeEar() as ear:
        ear.set_wake_recording_waits_for_alert(True)
        ear.set_wake_recording_waits_for_alert(False)


def test_echo_cancellation_is_off_until_it_is_asked_for():
    with edge_ear.EdgeEar() as ear:
        assert not ear.is_echo_cancellation_enabled
        ear.set_echo_cancellation(False)


def test_a_device_says_what_it_will_take():
    with edge_ear.EdgeEar() as ear:
        for formats in (ear.input_device_formats(), ear.output_device_formats()):
            assert formats
            for f in formats:
                assert f.channels > 0
                assert f.min_sample_rate <= f.max_sample_rate
                assert f.sample_type in ("i16", "f32")
            assert "Hz" in repr(formats[0])


def test_a_device_format_is_taken_or_refused_by_name():
    with edge_ear.EdgeEar() as ear:
        offered = ear.input_device_formats()[0]
        ear.set_input_device_format(
            sample_rate=offered.min_sample_rate,
            channels=offered.channels,
            sample_type=offered.sample_type,
        )
        assert ear.input_format is None          # 아직 안 열렸다
        ear.start()
        opened = ear.input_format
        assert opened.min_sample_rate == offered.min_sample_rate
        assert opened.min_sample_rate == opened.max_sample_rate
        ear.stop()

        ear.set_input_device_format()             # 인자 없이 = 장치가 고른다
        with pytest.raises(edge_ear.UnsupportedFormat) as caught:
            ear.set_input_device_format(sample_rate=12345, channels=7)
        assert "offers" in str(caught.value)      # 무엇을 받는지 말해준다


def test_the_rules_weighed_frame_by_frame_can_be_changed_mid_recording():
    with edge_ear.EdgeEar() as ear:
        ear.set_no_speech_timeout(30.0)
        ear.set_max_recording(60.0)
        ear.enable_speech()
        ear.start()
        ear.start_recording()

        import time
        deadline = time.time() + 2
        while not ear.is_recording and time.time() < deadline:
            time.sleep(0.005)
        assert ear.is_recording

        # Weighed every frame, so changing one mid-recording reaches it.
        # Moving a slider needs no recording stopped and reopened.
        ear.set_silence_duration(0.1)
        ear.set_speech_threshold(0.9)
        ear.set_max_recording(5.0)
        ear.set_no_speech_timeout(5.0)
        assert ear.is_recording

        # The exception: an open recording cannot reach further back,
        # so this is refused rather than quietly ignored.
        with pytest.raises(edge_ear.RecordingOpen):
            ear.set_pre_roll(0.2)
