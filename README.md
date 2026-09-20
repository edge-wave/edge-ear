# edge-ear

A local audio front end: listen to the microphone, notice a wake word,
notice when someone stops talking, and play a sound back.

Nothing leaves the machine. There is no network code in it and no place
to put any.

## What it does

- Reads one microphone and hands the audio to whoever asks, in whatever
  format each asks for
- Listens for one or more wake words at once and tells you which it
  heard
- Hands you a recording once the speaker goes quiet
- Plays a sound, so an alert or a spoken reply can come out of the
  speaker

## What it does not do

- Turn speech into text, write replies, or turn text into speech
- Reach the network, in any form
- Decide what happens next. That is your program's job
- Ship a wake word model. You supply the three files it needs
- Run on Windows yet

## Getting audio

```rust
use edge_ear_core::EdgeEar;

let ear = EdgeEar::new()?;
ear.start()?;
let chunk = ear.read(None)?;
```

That is the whole thing. Detectors are off until you switch them on:

```rust
ear.load_wake_features(&spectrogram, &features)?;
ear.add_wake_model(None, &phrase)?;
ear.enable_wake(None)?;
ear.enable_speech()?;

ear.on_event(|event| match event {
    Event::WakeDetected { word, .. } => println!("heard {word}"),
    Event::SpeechEnded { audio, reason, .. } => {
        println!("{} samples, ended on {reason}", audio.len())
    }
    _ => {}
})?;
```

The word is called after its file unless you name it, so a detection
of `hey_jarvis_v0.1.onnx` arrives as `hey_jarvis_v0.1`. Add more words
with `add_wake_model` to listen for them together: the two feature
models run once for all of them, so each extra word costs only its own
small model. Each word can have its own threshold through
`set_wake_word_threshold`.

## Models

| What | Who supplies it |
|------|-----------------|
| Speech detection | Ships with the library |
| Wake word | You do, all three files |

The ready-made wake word models are offered for non-commercial use, and
this library is MIT or Apache-2.0, so it cannot pass them on. See
[THIRD-PARTY-LICENSES](./THIRD-PARTY-LICENSES).

## Backends

The microphone and speaker sit behind a trait, so the device layer is
swappable. Each wrapper names the one it wants at build time:

| Feature | Reaches the devices through | Platforms |
|---------|-----------------------------|-----------|
| `cpal-backend` (default) | `cpal` | macOS, Linux |
| `tinypipewire-backend` | PipeWire, natively | Linux |

On Linux `cpal` already arrives at PipeWire through the ALSA
compatibility layer. The native backend is for when that indirection is
in the way: devices are named and numbered by the PipeWire graph, and
buffers are negotiated with it directly.

Exactly one is normally on. `EdgeEar::new()` takes cpal where it is
compiled in and tinypipewire otherwise, so nothing above the trait
changes:

```bash
cargo build -p edge-ear-core --no-default-features \
  --features tinypipewire-backend
```

## Building

```bash
sudo apt install libasound2-dev pkg-config   # Linux; macOS needs nothing
sudo apt install libpipewire-0.3-dev         # only for tinypipewire-backend
cargo build --workspace
cargo test --workspace
```

Tests that need a real microphone or a wake word model are marked
ignored:

```bash
cargo test --workspace -- --ignored
EDGE_EAR_WAKE_DIR=/path/to/models \
  EDGE_EAR_WAKE_WAV=/path/to/wake-word.wav \
  cargo test -p edge-ear-core -- --ignored
```

Try it out loud:

```bash
cargo run --example listen -- 30
```

## Python

```bash
cd py && maturin develop

# or against PipeWire
cd py && maturin develop \
  --no-default-features --features tinypipewire-backend
```

```python
from edge_ear import EdgeEar

with EdgeEar() as ear:
    ear.start()
    chunk = ear.read()
```

What the library logs arrives through Python's own `logging`, on
loggers under `edge_ear`:

```python
import logging

logging.basicConfig(level=logging.DEBUG)
```

Levels are read once and remembered, so the audio threads need not
reach into Python to find out whether a message would be printed.
Changing them after the library has logged something needs
`edge_ear.reset_logging()`.

## Licence

MIT or Apache-2.0, your choice.
