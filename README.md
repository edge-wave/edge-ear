# edge-ear

A local, real-time audio front end. It listens, it reacts, and it plays sounds.
It does not understand speech, and it does not talk to a network.

**Status: early. Nothing is built yet.** The design is settled and the work is
planned, but there is no code to use.

## What it does

- Captures microphone audio and hands it to your application on demand
- Spots a wake word and tells you
- Notices when the speaker has stopped talking, and hands you the recording
- Plays sounds: alerts, looping wait sounds, and audio that arrives while
  running

The microphone is read once, by one thread, and shared out to whoever wants it.
A slow reader never slows down anyone else, and your callbacks never run where
they could break the audio.

## What it does not do

- Turn speech into text, generate replies, or turn text into speech
- Reach the network. There is no client here, and no interface for one either
- Decide the order of steps in a conversation

Those belong to whatever uses this library. Keeping them out is what lets this
one stay small and stay quick.

## Where it runs

Linux and macOS.

## Licence

Dual licensed under either of:

- MIT ([LICENSE-MIT](LICENSE-MIT))
- Apache License 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.

Unless you say otherwise, any contribution you send in shall be dual licensed
the same way, with no extra terms.
