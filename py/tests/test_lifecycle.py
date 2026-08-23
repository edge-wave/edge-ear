"""Getting hold of the devices, and letting go of them again."""

import struct

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


def test_settings_are_refused_while_a_recording_is_open():
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

        with pytest.raises(edge_ear.RecordingOpen):
            ear.set_silence_duration(0.1)
