# edge-ear

A local audio front end: listen to the microphone, notice a wake word,
notice when someone stops talking, and play a sound back.

Nothing leaves the machine. There is no network code in it and no place
to put any.

## What it does

- Reads one microphone and hands the audio to whoever asks, in whatever
  format each asks for
- Listens for a wake word and tells you when it hears one
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
ear.load_wake_model(&phrase)?;
ear.enable_wake(None)?;
ear.enable_speech()?;

ear.on_event(|event| match event {
    Event::WakeDetected { .. } => println!("heard it"),
    Event::SpeechEnded { audio, reason, .. } => {
        println!("{} samples, ended on {reason}", audio.len())
    }
    _ => {}
})?;
```

## Models

| What | Who supplies it |
|------|-----------------|
| Speech detection | Ships with the library |
| Wake word | You do, all three files |

The ready-made wake word models are offered for non-commercial use, and
this library is MIT or Apache-2.0, so it cannot pass them on. See
[THIRD-PARTY-LICENSES](./THIRD-PARTY-LICENSES).

## Building

```bash
sudo apt install libasound2-dev pkg-config   # Linux
cargo build --workspace
cargo test --workspace
```

Tests that need a real microphone or a wake word model are marked
ignored:

```bash
cargo test --workspace -- --ignored
EDGE_EAR_WAKE_DIR=/path/to/models cargo test -p edge-ear-core -- --ignored
```

Try it out loud:

```bash
cargo run --example listen -- 30
```

## Python

```bash
cd py && maturin develop
```

```python
from edge_ear import EdgeEar

with EdgeEar() as ear:
    ear.start()
    chunk = ear.read()
```

## Licence

MIT or Apache-2.0, your choice.
