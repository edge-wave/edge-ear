//! The README's code, compiled so it cannot drift out of date.

use edge_ear_core::EdgeEar;
use edge_ear_core::backend::fake::FakeBackend;
use edge_ear_core::echo::BuiltinCanceller;
use edge_ear_core::error::Result;
use edge_ear_core::events::Event;
use std::path::Path;

/// The three calls the README opens with.
#[test]
fn the_readme_first_example_works() -> Result<()> {
    // A real application writes EdgeEar::new(); a test supplies a
    // device so it runs anywhere.
    let ear = EdgeEar::with_backend(Box::new(FakeBackend::paced()))?;
    ear.start()?;
    let chunk = ear.read(None)?;

    assert!(!chunk.samples.is_empty());
    ear.stop()
}

/// The second block. Compiled, not run, because it needs models the
/// library does not ship.
#[allow(dead_code)]
fn the_readme_second_example(
    ear: &EdgeEar,
    spectrogram: &Path,
    features: &Path,
    phrase: &Path,
) -> Result<()> {
    ear.load_wake_features(spectrogram, features)?;
    ear.add_wake_model(None, phrase)?;
    ear.enable_wake(None)?;
    ear.enable_speech()?;

    ear.on_event(|event| match event {
        Event::WakeDetected { word, .. } => println!("heard {word}"),
        Event::SpeechEnded { audio, reason, .. } => {
            println!("{} samples, ended on {reason}", audio.len())
        }
        _ => {}
    })
}

/// The echo cancellation block. Compiled, not run, because only a build
/// with the webrtc-aec feature accepts it.
#[allow(dead_code)]
fn the_readme_echo_example(ear: &EdgeEar) -> Result<()> {
    ear.set_echo_canceller(BuiltinCanceller::Webrtc)?; // before start
    ear.start()?;
    Ok(())
}
