//! Callback delivery, kept away from the audio path. Producers only
//! enqueue; one thread drains and runs the handler, so a handler that
//! blocks delays later events and nothing else.

use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use super::Event;

/// How many events wait before the oldest is dropped. Bounded on
/// purpose: a handler that never returns must not grow memory.
pub const DEFAULT_QUEUE_CAPACITY: usize = 256;

/// `Sync` as well as `Send`, because the dispatcher holds the handler
/// in shared state while the calling thread may replace it.
type Handler = Box<dyn Fn(Event) + Send + Sync>;

struct Queue {
    items: VecDeque<Event>,
    capacity: usize,
    dropped: u64,
    closed: bool,
}

pub struct Dispatcher {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
    handler: Mutex<Option<Arc<Handler>>>,
}

impl Dispatcher {
    pub fn new(capacity: usize) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                items: VecDeque::new(),
                capacity: capacity.max(1),
                dropped: 0,
                closed: false,
            }),
            ready: Condvar::new(),
            handler: Mutex::new(None),
        });

        let worker = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("edge-ear-dispatch".to_string())
                .spawn(move || run(shared))
                .expect("dispatcher thread must start")
        };

        Self {
            shared,
            worker: Mutex::new(Some(worker)),
        }
    }

    /// Replace the handler. Events raised before one is set stay
    /// queued, so setting it a moment late loses nothing.
    pub fn set_handler(&self, handler: Handler) {
        // Held because the dispatcher decides under this lock whether
        // to park, and a wake before that wait would be lost.
        let queue = lock(&self.shared.queue);
        *lock(&self.shared.handler) = Some(Arc::new(handler));
        drop(queue);
        self.shared.ready.notify_all();
    }

    /// Queue an event. Never blocks and never fails, so it is safe to
    /// call from the capture and inference threads.
    pub fn emit(&self, event: Event) {
        let mut queue = lock(&self.shared.queue);
        if queue.closed {
            return;
        }
        let mut first_drop = false;
        if queue.items.len() >= queue.capacity {
            queue.items.pop_front();
            queue.dropped += 1;
            first_drop = queue.dropped == 1;
        }
        queue.items.push_back(event);
        drop(queue);
        self.shared.ready.notify_one();
        // This warns once per run of drops, so a stuck handler cannot flood the log.
        if first_drop {
            log::warn!("the event queue is full, so the oldest events are being dropped");
        }
    }

    #[cfg(test)]
    pub fn queued(&self) -> usize {
        lock(&self.shared.queue).items.len()
    }

    #[cfg(test)]
    pub fn dropped(&self) -> u64 {
        lock(&self.shared.queue).dropped
    }

    /// Stop the dispatcher and wait for it. Called from inside a
    /// handler this would deadlock, so that case is detected and the
    /// thread left to finish on its own.
    pub fn shutdown(&self) {
        lock(&self.shared.queue).closed = true;
        self.shared.ready.notify_all();

        let is_dispatcher = thread::current().name() == Some("edge-ear-dispatch");
        let handle = lock(&self.worker).take();
        if let Some(handle) = handle {
            if is_dispatcher {
                // Joining ourselves would hang. The thread is already
                // told to stop and will exit once this handler returns.
                return;
            }
            crate::join_worker(handle, "event dispatch");
        }
    }
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(shared: Arc<Shared>) {
    loop {
        let next = {
            let mut queue = lock(&shared.queue);
            loop {
                // Take nothing until there is somewhere to deliver it.
                // Draining into a missing handler throws events away,
                // and a handler set a moment later would miss them.
                let ready = lock(&shared.handler).is_some() && !queue.items.is_empty();
                if ready {
                    let event = queue.items.pop_front().expect("checked above");
                    let dropped = std::mem::take(&mut queue.dropped);
                    break Some((event, dropped));
                }
                if queue.closed {
                    break None;
                }
                queue = shared
                    .ready
                    .wait(queue)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };

        let Some((event, dropped)) = next else { return };

        // Clone the handle out before calling, so the handler may set a
        // new handler or emit without fighting us for the lock.
        let handler = lock(&shared.handler).clone();
        if let Some(handler) = handler {
            if dropped > 0 {
                deliver(&handler, Event::EventsDropped { count: dropped });
            }
            deliver(&handler, event);
        }
    }
}

/// Run the handler, surviving a panic in it. Unwinding out of here would
/// end this thread, and every later event would be lost without a word.
fn deliver(handler: &Handler, event: Event) {
    let kind = event.kind();
    if panic::catch_unwind(AssertUnwindSafe(|| handler(event))).is_err() {
        log::error!("the event handler panicked on {kind}; later events are still delivered");
    }
}

/// A poisoned lock means a handler panicked. The queue is still sound,
/// so keep going rather than taking the whole library down.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn collector() -> (Dispatcher, Arc<Mutex<Vec<String>>>) {
        let dispatcher = Dispatcher::new(DEFAULT_QUEUE_CAPACITY);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        dispatcher.set_handler(Box::new(move |event| {
            lock(&sink).push(event.kind().to_string());
        }));
        (dispatcher, seen)
    }

    fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            thread::sleep(Duration::from_millis(1));
        }
        false
    }

    #[test]
    fn events_arrive_in_order() {
        let (dispatcher, seen) = collector();
        dispatcher.emit(Event::WakeDetected {
            word: "w".into(),
            score: 0.9,
        });
        dispatcher.emit(Event::SoundFinished {
            id: "alert".to_string(),
        });

        assert!(wait_until(|| lock(&seen).len() == 2));
        assert_eq!(
            *lock(&seen),
            vec!["wake detected".to_string(), "sound finished".to_string()]
        );
    }

    #[test]
    fn events_raised_before_a_handler_exists_are_kept() {
        let dispatcher = Dispatcher::new(DEFAULT_QUEUE_CAPACITY);
        dispatcher.emit(Event::WakeDetected {
            word: "w".into(),
            score: 0.9,
        });
        dispatcher.emit(Event::SoundFinished {
            id: "alert".to_string(),
        });

        // Nothing has been taken yet, because there was nowhere to
        // deliver it.
        assert_eq!(dispatcher.queued(), 2);

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        dispatcher.set_handler(Box::new(move |event| {
            lock(&sink).push(event.kind().to_string());
        }));

        assert!(wait_until(|| lock(&seen).len() == 2), "{:?}", lock(&seen));
    }

    #[test]
    fn emit_never_blocks_even_with_a_stuck_handler() {
        let dispatcher = Dispatcher::new(8);
        let entered = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&entered);
        dispatcher.set_handler(Box::new(move |_| {
            flag.fetch_add(1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(200));
        }));

        // Far more events than the queue holds. If emit could block on
        // the handler, this would take many seconds.
        let started = Instant::now();
        for _ in 0..1_000 {
            dispatcher.emit(Event::WakeDetected {
                word: "w".into(),
                score: 0.5,
            });
        }
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "emit blocked for {:?}",
            started.elapsed()
        );
        assert!(wait_until(|| entered.load(Ordering::SeqCst) > 0));
    }

    #[test]
    fn a_full_queue_drops_oldest_and_reports_the_count() {
        let dispatcher = Dispatcher::new(4);
        for _ in 0..100 {
            dispatcher.emit(Event::WakeDetected {
                word: "w".into(),
                score: 0.5,
            });
        }
        assert!(dispatcher.dropped() > 0);
        assert!(dispatcher.queued() <= 4);

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        dispatcher.set_handler(Box::new(move |event| {
            lock(&sink).push(event.kind().to_string());
        }));

        assert!(wait_until(
            || lock(&seen).contains(&"events dropped".to_string())
        ));
    }

    #[test]
    fn a_panicking_handler_does_not_stop_later_events() {
        let dispatcher = Dispatcher::new(16);
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        dispatcher.set_handler(Box::new(move |_| {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("the first event makes the handler panic");
            }
        }));

        dispatcher.emit(Event::WakeDetected {
            word: "w".into(),
            score: 0.1,
        });
        assert!(wait_until(|| count.load(Ordering::SeqCst) == 1));
        dispatcher.emit(Event::WakeDetected {
            word: "w".into(),
            score: 0.2,
        });
        assert!(wait_until(|| count.load(Ordering::SeqCst) == 2));
    }

    #[test]
    fn shutdown_from_inside_a_handler_does_not_hang() {
        let dispatcher = Arc::new(Dispatcher::new(4));
        let inner = Arc::clone(&dispatcher);
        dispatcher.set_handler(Box::new(move |_| {
            // The reentrant case: this must return rather than joining
            // the thread it is running on.
            inner.shutdown();
        }));
        dispatcher.emit(Event::WakeDetected {
            word: "w".into(),
            score: 0.5,
        });
        assert!(wait_until(|| lock(&dispatcher.worker).is_none()));
    }
}
