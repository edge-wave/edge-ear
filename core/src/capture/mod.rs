pub mod convert;
pub mod ring;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::backend::InputStream;
use crate::capture::convert::{Converter, FrameAccumulator};
use crate::capture::ring::Ring;
use crate::config::{AudioFormat, Device, SampleType};
use crate::error::{Error, Result};
use crate::events::Event;
use crate::events::dispatch::Dispatcher;

/// Audio samples in whichever form the consumer asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Samples {
    I16(Vec<i16>),
    F32(Vec<f32>),
}

impl Samples {
    pub fn len(&self) -> usize {
        match self {
            Samples::I16(v) => v.len(),
            Samples::F32(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn sample_type(&self) -> SampleType {
        match self {
            Samples::I16(_) => SampleType::I16,
            Samples::F32(_) => SampleType::F32,
        }
    }

    pub fn as_i16(&self) -> Option<&[i16]> {
        match self {
            Samples::I16(v) => Some(v),
            Samples::F32(_) => None,
        }
    }

    pub fn as_f32(&self) -> Option<&[f32]> {
        match self {
            Samples::F32(v) => Some(v),
            Samples::I16(_) => None,
        }
    }
}

/// One block of live audio, delivered to one consumer.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioChunk {
    pub samples: Samples,
    pub format: AudioFormat,
    /// When the device produced this, not when it was read. A slow
    /// consumer still sees correct timing.
    pub captured_at: Instant,
    /// Where the first sample sits on the capture timeline, counted
    /// from the start of capture. Every consumer shares this timeline.
    pub position: Duration,
    /// Chunks this consumer lost before this one. Above zero only after
    /// it fell behind.
    pub dropped_before: u64,
}

impl AudioChunk {
    pub fn new(
        samples: Samples,
        format: AudioFormat,
        captured_at: Instant,
        position: Duration,
    ) -> Self {
        Self {
            samples,
            format,
            captured_at,
            position,
            dropped_before: 0,
        }
    }

    /// Where the last sample ends on the capture timeline.
    pub fn end(&self) -> Duration {
        self.position + self.duration()
    }

    /// How long this chunk covers.
    pub fn duration(&self) -> std::time::Duration {
        let frames = self.samples.len() / self.format.channels.max(1) as usize;
        std::time::Duration::from_secs_f64(frames as f64 / self.format.sample_rate as f64)
    }
}

/// The three independent readers of live audio. They share nothing but
/// the chunks handed to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConsumerKind {
    Wake,
    Speech,
    Read,
}

/// One independent reader, with its own queue, its own format, and its
/// own switch. Nothing here is shared with another consumer.
pub struct Consumer {
    pub(crate) kind: ConsumerKind,
    pub(crate) ring: Arc<Ring<AudioChunk>>,
    pub(crate) format: AudioFormat,
    /// Samples per delivery. `None` hands over whatever the device
    /// block produced, which is what the read path wants.
    pub(crate) frame_samples: Option<usize>,
    /// Off means the capture thread skips it entirely. Flipping this
    /// takes effect on the next block and disturbs nobody else.
    pub(crate) enabled: Arc<AtomicBool>,
}

/// Cloning shares the queue and the switch rather than copying them.
/// That is how the handle and the capture thread hold the same
/// consumer: one flips the switch, the other feeds the queue.
impl Clone for Consumer {
    fn clone(&self) -> Self {
        Self {
            kind: self.kind,
            ring: Arc::clone(&self.ring),
            format: self.format,
            frame_samples: self.frame_samples,
            enabled: Arc::clone(&self.enabled),
        }
    }
}

impl Consumer {
    pub fn new(
        kind: ConsumerKind,
        format: AudioFormat,
        frame_samples: Option<usize>,
        ring_capacity: Duration,
        enabled: bool,
    ) -> Self {
        Self {
            kind,
            ring: Arc::new(Ring::new(ring_chunks(format, frame_samples, ring_capacity))),
            format,
            frame_samples,
            enabled: Arc::new(AtomicBool::new(enabled)),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Flipping this takes effect on the next block of audio and
    /// disturbs no other consumer.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }
}

/// How many chunks a consumer's queue must hold to cover the wanted
/// history. Sized from time rather than a bare number, so pre-roll and
/// capacity stay tied together.
fn ring_chunks(format: AudioFormat, frame_samples: Option<usize>, capacity: Duration) -> usize {
    // Without a fixed frame size, assume a modest device block so the
    // queue still covers roughly the wanted time.
    let per_chunk = frame_samples.unwrap_or((format.sample_rate as usize / 100).max(1));
    let total = (capacity.as_secs_f64() * format.sample_rate as f64) as usize;
    (total / per_chunk.max(1)).clamp(8, 4096)
}

/// The one owner of the microphone.
///
/// It reads the device once and hands every block to each enabled
/// consumer, never waiting on one, so no consumer can slow another.
///
/// What this loop may do, checked against what it calls:
///
/// - **Waiting**: only on the device, in `stream.read`. Handing over a
///   block and raising a notification both drop the oldest instead.
///   The waiting form of the queue feeds the speaker and belongs
///   nowhere near here.
/// - **Locks**: held only long enough to move one item, never across
///   user code, and no user code runs on this thread at all.
/// - **Allocation**: one block per consumer, per block. Bounded, and
///   not growing. Pooling would remove it, if measurement ever asks.
pub struct CaptureThread {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    rings: Vec<Arc<Ring<AudioChunk>>>,
}

impl CaptureThread {
    pub fn start(
        stream: Box<dyn InputStream>,
        consumers: Vec<Consumer>,
        dispatcher: Arc<Dispatcher>,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let rings = consumers.iter().map(|c| Arc::clone(&c.ring)).collect();

        let worker = {
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("edge-ear-capture".to_string())
                .spawn(move || run(stream, consumers, dispatcher, stop))
                .map_err(|e| Error::Backend {
                    device: Device::Input,
                    reason: format!("capture thread would not start: {e}"),
                })?
        };

        Ok(Self {
            stop,
            worker: Some(worker),
            rings,
        })
    }

    /// Stop reading and release every waiting reader.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for ring in &self.rings {
            ring.close();
        }
        if let Some(worker) = self.worker.take() {
            crate::join_worker(worker, "capture");
        }
    }
}

impl Drop for CaptureThread {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(
    mut stream: Box<dyn InputStream>,
    consumers: Vec<Consumer>,
    dispatcher: Arc<Dispatcher>,
    stop: Arc<AtomicBool>,
) {
    let device_format = stream.format();

    // One converter per wanted format, not one per consumer. Two
    // consumers asking for the same thing share the work.
    let mut formats: Vec<AudioFormat> = Vec::new();
    for consumer in &consumers {
        if !formats.contains(&consumer.format) {
            formats.push(consumer.format);
        }
    }

    let mut converters = Vec::with_capacity(formats.len());
    for format in &formats {
        match Converter::new(device_format, *format) {
            Ok(c) => converters.push(c),
            Err(e) => {
                log::error!("capture cannot convert {device_format:?} to {format:?}: {e}");
                dispatcher.emit(Event::DeviceError {
                    device: Device::Input,
                    message: e.to_string(),
                });
                return;
            }
        }
    }

    let slot_of: Vec<usize> = consumers
        .iter()
        .map(|c| {
            formats
                .iter()
                .position(|f| *f == c.format)
                .expect("every format was collected above")
        })
        .collect();

    // Samples each converter has produced since capture began. They
    // place every chunk on the timeline, whether its consumer is on or not.
    let mut produced: Vec<u64> = vec![0; converters.len()];

    let mut accumulators: Vec<FrameAccumulator> = consumers
        .iter()
        .map(|c| FrameAccumulator::new(c.frame_samples))
        .collect();
    log::debug!("capture thread running: device {device_format:?}, converting to {formats:?}");

    // A conversion failure is logged when it starts, not on every block.
    let mut converting_failed = false;

    while !stop.load(Ordering::Relaxed) {
        let block = match stream.read() {
            Ok(block) => block,
            Err(Error::Stopped) => break,
            Err(e) => {
                log::error!("the microphone stopped delivering audio: {e}");
                dispatcher.emit(Event::DeviceError {
                    device: Device::Input,
                    message: e.to_string(),
                });
                break;
            }
        };
        let captured_at = Instant::now();

        // Convert once per wanted format.
        let mut converted: Vec<Option<Samples>> = Vec::with_capacity(converters.len());
        let mut failed_now = false;
        for converter in converters.iter_mut() {
            match converter.convert(&block) {
                Ok(samples) => converted.push(Some(samples)),
                Err(e) => {
                    // Told once when it starts, not for every block: a
                    // failure that repeats would fill the event queue.
                    if !converting_failed {
                        log::error!("captured audio could not be converted: {e}");
                        dispatcher.emit(Event::DeviceError {
                            device: Device::Input,
                            message: e.to_string(),
                        });
                    }
                    failed_now = true;
                    converted.push(None);
                }
            }
        }
        for (slot, samples) in converted.iter().enumerate() {
            if let Some(samples) = samples {
                produced[slot] += samples.len() as u64;
            }
        }
        if converting_failed && !failed_now {
            log::info!("captured audio converts again");
        }
        converting_failed = failed_now;

        for (index, consumer) in consumers.iter().enumerate() {
            if !consumer.is_enabled() {
                // Still drop whatever it would have received, so it does
                // not resume with stale audio.
                accumulators[index].clear();
                continue;
            }
            let slot = slot_of[index];
            let Some(samples) = converted[slot].as_ref() else {
                continue;
            };
            // Frames leave the accumulator in order, starting with the
            // samples it was already holding.
            let before = produced[slot] - samples.len() as u64;
            let mut start = before - accumulators[index].held() as u64;
            for frame in accumulators[index].push(samples.clone()) {
                let len = frame.len() as u64;
                let position = timeline(start, consumer.format);
                consumer.ring.push(AudioChunk::new(
                    frame,
                    consumer.format,
                    captured_at,
                    position,
                ));
                start += len;
            }
        }
    }

    let _ = stream.stop();
    for consumer in &consumers {
        consumer.ring.close();
    }
    log::debug!("capture thread finished");
}

/// The time of one sample on the capture timeline.
fn timeline(sample: u64, format: AudioFormat) -> Duration {
    let frame = sample / format.channels.max(1) as u64;
    let nanos = frame as u128 * 1_000_000_000 / format.sample_rate.max(1) as u128;
    Duration::from_nanos(nanos as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_timeline_counts_frames_at_the_format_rate() {
        let stereo = AudioFormat::new(16_000, 2, SampleType::I16);
        assert_eq!(timeline(3_200, stereo), Duration::from_millis(100));
        assert_eq!(
            timeline(1, AudioFormat::mono_16k()),
            Duration::from_nanos(62_500)
        );
    }

    /// Hands over canned blocks, then says the stream has ended.
    struct Canned {
        blocks: std::collections::VecDeque<Samples>,
        format: AudioFormat,
    }

    impl InputStream for Canned {
        fn format(&self) -> AudioFormat {
            self.format
        }
        fn read(&mut self) -> Result<Samples> {
            self.blocks.pop_front().ok_or(Error::Stopped)
        }
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn drain(ring: &Ring<AudioChunk>) -> Vec<AudioChunk> {
        std::iter::from_fn(|| ring.take(Some(Duration::from_secs(5))).ok().map(|t| t.item))
            .collect()
    }

    /// Where the audio first turns loud, on the capture timeline.
    fn onset(chunks: &[AudioChunk]) -> Duration {
        for chunk in chunks {
            let samples = chunk.samples.as_i16().unwrap();
            if let Some(i) = samples.iter().position(|s| s.abs() > 4_000) {
                let frames = (i / chunk.format.channels as usize) as u64;
                return chunk.position
                    + Duration::from_nanos(
                        frames * 1_000_000_000 / chunk.format.sample_rate as u64,
                    );
            }
        }
        panic!("the audio never turned loud");
    }

    #[test]
    fn every_consumer_places_the_same_sound_at_the_same_time() {
        // One second at 48 kHz: quiet, then loud from 500 ms on.
        let device = AudioFormat::new(48_000, 1, SampleType::I16);
        let audio: Vec<i16> = (0..48_000)
            .map(|i| if i < 24_000 { 0 } else { 8_000 })
            .collect();
        let blocks = audio
            .chunks(480)
            .map(|b| Samples::I16(b.to_vec()))
            .collect();
        let stream = Box::new(Canned {
            blocks,
            format: device,
        });

        let read = Consumer::new(
            ConsumerKind::Read,
            device,
            None,
            Duration::from_secs(5),
            true,
        );
        let wake_format = AudioFormat::mono_16k();
        let wake = Consumer::new(
            ConsumerKind::Wake,
            wake_format,
            Some(1280),
            Duration::from_secs(5),
            true,
        );
        let dispatcher = Arc::new(Dispatcher::new(
            crate::events::dispatch::DEFAULT_QUEUE_CAPACITY,
        ));
        let mut capture =
            CaptureThread::start(stream, vec![read.clone(), wake.clone()], dispatcher).unwrap();

        let read_chunks = drain(&read.ring);
        let wake_chunks = drain(&wake.ring);
        capture.stop();

        for chunks in [&read_chunks, &wake_chunks] {
            for pair in chunks.windows(2) {
                let gap = pair[1].position.abs_diff(pair[0].end());
                assert!(
                    gap < Duration::from_micros(100),
                    "chunks are not back to back: {gap:?}"
                );
            }
        }
        let heard = onset(&read_chunks);
        assert_eq!(heard, Duration::from_millis(500));
        let resampled = onset(&wake_chunks);
        assert!(
            heard.abs_diff(resampled) < Duration::from_millis(2),
            "read hears it at {heard:?}, wake at {resampled:?}"
        );
    }

    #[test]
    fn duration_follows_the_format() {
        let chunk = AudioChunk::new(
            Samples::I16(vec![0; 1600]),
            AudioFormat::mono_16k(),
            Instant::now(),
            Duration::from_millis(50),
        );
        assert_eq!(chunk.duration(), std::time::Duration::from_millis(100));
        assert_eq!(chunk.end(), Duration::from_millis(150));
    }

    #[test]
    fn stereo_duration_counts_frames_not_samples() {
        let format = AudioFormat::new(16_000, 2, SampleType::I16);
        let chunk = AudioChunk::new(
            Samples::I16(vec![0; 3200]),
            format,
            Instant::now(),
            Duration::ZERO,
        );
        assert_eq!(chunk.duration(), std::time::Duration::from_millis(100));
    }
}
