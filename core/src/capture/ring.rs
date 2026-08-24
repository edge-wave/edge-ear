use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// One consumer's private queue of audio. The writer never waits: when
/// full, the oldest goes and a counter rises, so a slow consumer loses
/// only its own audio and holds up nobody else.
pub struct Ring<T> {
    inner: Mutex<Inner<T>>,
    ready: Condvar,
    /// Woken when an item leaves. Only the playback side waits on this:
    /// filling the speaker faster than it drains would throw audio
    /// away, so there the writer does wait.
    space: Condvar,
}

struct Inner<T> {
    items: VecDeque<T>,
    capacity: usize,
    /// Dropped since this consumer last took an item.
    dropped_pending: u64,
    /// Dropped over the whole life of the ring.
    dropped_total: u64,
    closed: bool,
}

/// An item plus how much this consumer lost before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Taken<T> {
    pub item: T,
    pub dropped_before: u64,
}

impl<T> Ring<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "a ring with no room could never deliver");
        Self {
            inner: Mutex::new(Inner {
                items: VecDeque::with_capacity(capacity),
                capacity,
                dropped_pending: 0,
                dropped_total: 0,
                closed: false,
            }),
            ready: Condvar::new(),
            space: Condvar::new(),
        }
    }

    /// Add an item. Never blocks, never fails. Drops the oldest item
    /// when full.
    pub fn push(&self, item: T) {
        let mut inner = self.lock();
        if inner.items.len() == inner.capacity {
            inner.items.pop_front();
            inner.dropped_pending += 1;
            inner.dropped_total += 1;
        }
        inner.items.push_back(item);
        drop(inner);
        self.ready.notify_one();
    }

    /// Add an item, waiting for room instead of dropping the oldest.
    /// For the speaker only, where discarding unplayed audio is heard;
    /// never from a device callback or the capture thread.
    #[allow(dead_code, reason = "used by device backends")]
    pub fn push_before(&self, item: T, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut inner = self.lock();
        loop {
            if inner.closed {
                return Err(Error::Stopped);
            }
            if inner.items.len() < inner.capacity {
                inner.items.push_back(item);
                drop(inner);
                self.ready.notify_one();
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout);
            }
            let (guard, wait) = self
                .space
                .wait_timeout(inner, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner = guard;
            if wait.timed_out() && inner.items.len() >= inner.capacity && !inner.closed {
                return Err(Error::Timeout);
            }
        }
    }

    /// Take the next item, waiting if there is none. `None` waits until
    /// one arrives or the ring closes; `Timeout` when the limit passes,
    /// `Stopped` when it closed while waiting.
    pub fn take(&self, timeout: Option<Duration>) -> Result<Taken<T>> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut inner = self.lock();

        loop {
            if let Some(item) = inner.items.pop_front() {
                let dropped_before = std::mem::take(&mut inner.dropped_pending);
                drop(inner);
                self.space.notify_one();
                return Ok(Taken {
                    item,
                    dropped_before,
                });
            }
            if inner.closed {
                return Err(Error::Stopped);
            }

            inner = match deadline {
                None => self.ready.wait(inner).unwrap_or_else(|e| e.into_inner()),
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(Error::Timeout);
                    }
                    let (guard, wait) = self
                        .ready
                        .wait_timeout(inner, left)
                        .unwrap_or_else(|e| e.into_inner());
                    if wait.timed_out() && guard.items.is_empty() && !guard.closed {
                        return Err(Error::Timeout);
                    }
                    guard
                }
            };
        }
    }

    /// Take an item only if one is already there. A device callback
    /// must never wait, so this is how it drains the queue.
    #[allow(dead_code, reason = "used by device backends")]
    pub fn try_take(&self) -> Option<Taken<T>> {
        let mut inner = self.lock();
        let item = inner.items.pop_front()?;
        let dropped_before = std::mem::take(&mut inner.dropped_pending);
        drop(inner);
        self.space.notify_one();
        Some(Taken {
            item,
            dropped_before,
        })
    }

    /// Release every waiting reader with `Stopped`. Items already
    /// queued stay readable.
    pub fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_all();
        self.space.notify_all();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    #[cfg(test)]
    pub fn dropped_total(&self) -> u64 {
        self.lock().dropped_total
    }

    /// A poisoned lock means another thread panicked while holding it.
    /// The queue itself is still consistent, so carry on rather than
    /// panicking again and taking the audio path down.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<T>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl<T> Ring<T> {
    /// Snapshot of everything held, oldest first, without removing it.
    #[allow(dead_code, reason = "pre-roll will read history through this")]
    pub fn snapshot(&self) -> Vec<T>
    where
        T: Clone,
    {
        self.lock().items.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn delivers_in_order() {
        let ring = Ring::new(4);
        ring.push(1);
        ring.push(2);
        assert_eq!(ring.take(None).unwrap().item, 1);
        assert_eq!(ring.take(None).unwrap().item, 2);
    }

    #[test]
    fn full_ring_drops_oldest_and_counts_it() {
        let ring = Ring::new(2);
        for n in 1..=5 {
            ring.push(n);
        }
        // 1, 2, 3 were pushed out by 4 and 5.
        let taken = ring.take(None).unwrap();
        assert_eq!(taken.item, 4);
        assert_eq!(taken.dropped_before, 3);

        // The count resets once reported, so it is not counted twice.
        let taken = ring.take(None).unwrap();
        assert_eq!(taken.item, 5);
        assert_eq!(taken.dropped_before, 0);
    }

    #[test]
    fn writer_never_blocks_on_a_full_ring() {
        let ring = Ring::new(1);
        // Far more pushes than capacity, with nobody reading. If the
        // writer could block, this would hang instead of finishing.
        for n in 0..10_000 {
            ring.push(n);
        }
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.dropped_total(), 9_999);
    }

    #[test]
    fn a_slow_consumer_does_not_affect_another_ring() {
        let slow = Ring::new(2);
        let fast = Ring::new(2);
        for n in 0..100 {
            slow.push(n);
            fast.push(n);
            // The fast one keeps up; the slow one never reads.
            assert_eq!(fast.take(None).unwrap().item, n);
        }
        assert_eq!(fast.dropped_total(), 0, "the fast ring lost nothing");
        assert!(slow.dropped_total() > 0, "the slow ring lost its own audio");
    }

    #[test]
    fn timeout_is_reported_when_nothing_arrives() {
        let ring: Ring<u8> = Ring::new(2);
        let err = ring.take(Some(Duration::from_millis(20))).unwrap_err();
        assert!(matches!(err, Error::Timeout), "{err}");
    }

    #[test]
    fn waiting_reader_is_released_by_close() {
        let ring: Arc<Ring<u8>> = Arc::new(Ring::new(2));
        let reader = {
            let ring = Arc::clone(&ring);
            thread::spawn(move || ring.take(None))
        };
        thread::sleep(Duration::from_millis(50));
        ring.close();
        let err = reader.join().unwrap().unwrap_err();
        assert!(matches!(err, Error::Stopped), "{err}");
    }

    #[test]
    fn queued_items_survive_close() {
        let ring = Ring::new(2);
        ring.push(7);
        ring.close();
        assert_eq!(ring.take(None).unwrap().item, 7);
        assert!(matches!(ring.take(None).unwrap_err(), Error::Stopped));
    }

    #[test]
    fn snapshot_leaves_the_queue_alone() {
        let ring = Ring::new(4);
        ring.push(1);
        ring.push(2);
        assert_eq!(ring.snapshot(), vec![1, 2]);
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn try_take_does_not_wait() {
        let ring: Ring<u8> = Ring::new(2);
        assert!(ring.try_take().is_none());
        ring.push(9);
        assert_eq!(ring.try_take().unwrap().item, 9);
    }
}
