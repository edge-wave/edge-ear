"""What the binding adds on top of the core: the interpreter lock.

The thread that reads the microphone never touches Python. The thread
that delivers notifications holds the lock only for the length of a
handler. A read lets go of the lock while it waits.
"""

import threading
import time

import edge_ear


def test_a_read_does_not_stop_other_python_threads():
    """The lock is let go of while a read waits, so other threads run.

    Reading in a loop drains the queue, after which each read waits for
    the device to produce the next block. If the lock were held through
    those waits, the counting thread below would be starved.
    """
    with edge_ear.EdgeEar() as ear:
        ear.start()

        ticks = []
        stop = threading.Event()

        def counter():
            while not stop.is_set():
                ticks.append(1)
                time.sleep(0.002)

        worker = threading.Thread(target=counter)
        worker.start()
        try:
            before = len(ticks)
            reads = 0
            deadline = time.time() + 0.5
            while time.time() < deadline:
                try:
                    ear.read(timeout=0.2)
                    reads += 1
                except edge_ear.EdgeEarError:
                    break
            after = len(ticks)
        finally:
            stop.set()
            worker.join()

        assert reads > 0, "no audio arrived at all, so nothing was tested"
        assert after - before > 20, (
            f"the other thread only ran {after - before} times "
            f"across {reads} reads"
        )


def test_a_handler_that_raises_does_not_stop_later_ones():
    with edge_ear.EdgeEar() as ear:
        seen = []

        def handler(event):
            seen.append(event)
            raise RuntimeError("this handler is badly behaved")

        ear.on_event(handler)
        ear.register_sound("beep", pcm=[2000] * 1600)

        for _ in range(3):
            ear.play_sound("beep")
            time.sleep(0.25)

        deadline = time.time() + 5
        while len(seen) < 3 and time.time() < deadline:
            time.sleep(0.01)

        assert len(seen) >= 3, f"delivery stopped after {len(seen)} events"


def test_a_handler_can_call_back_into_the_handle():
    with edge_ear.EdgeEar() as ear:
        done = threading.Event()

        def handler(event):
            # All of this runs inside a handler and must not deadlock.
            ear.is_running
            ear.is_recording
            ear.input_devices()
            done.set()

        ear.on_event(handler)
        ear.register_sound("beep", pcm=[2000] * 1600)
        ear.play_sound("beep")

        assert done.wait(timeout=5), "calling back into the handle deadlocked"


def test_calls_from_many_threads_at_once_are_safe():
    with edge_ear.EdgeEar() as ear:
        ear.start()
        errors = []

        def hammer(n):
            try:
                for _ in range(30):
                    if n % 2:
                        ear.read(timeout=0.05)
                    else:
                        ear.is_running
                        ear.enable_speech()
                        ear.disable_speech()
            except edge_ear.EdgeEarError:
                pass          # a timeout here is fine
            except Exception as e:
                errors.append(e)

        threads = [threading.Thread(target=hammer, args=(n,)) for n in range(4)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        assert not errors, f"threads hit {errors}"


def test_events_arrive_as_their_own_classes():
    with edge_ear.EdgeEar() as ear:
        seen = []
        ear.on_event(seen.append)
        ear.register_sound("beep", pcm=[2000] * 800)
        ear.play_sound("beep")

        deadline = time.time() + 5
        while not seen and time.time() < deadline:
            time.sleep(0.01)

        assert seen, "no notification arrived"
        event = seen[0]
        assert isinstance(event, edge_ear.SoundFinished)
        assert isinstance(event, edge_ear.AudioEvent)
        assert event.id == "beep"
