//! Event stream — a pollable, non-blocking event queue with overflow accounting.
//!
//! The substrate under eventfd/signalfd/timerfd/inotify, DRM events, GPIO line
//! events, and netlink async notifications: a producer posts records, a consumer
//! drains them without blocking, and when the buffer overflows the *lost* count
//! is tracked rather than blocking the producer or corrupting the stream.
//!
//! It is built by composing [`Ring`] — a primitive made of a primitive. The
//! only policy it adds over the bare ring is the overflow rule (drop newest and
//! count, like inotify's `IN_Q_OVERFLOW`).
//!
//! [`Ring`]: crate::ring::Ring

use crate::ring::Ring;

/// A bounded, non-blocking event queue of `N` records of type `T`.
#[derive(Debug)]
pub struct EventQueue<T, const N: usize> {
    ring: Ring<T, N>,
    lost: u64,
}

impl<T, const N: usize> Default for EventQueue<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> EventQueue<T, N> {
    /// Create an empty queue.
    pub const fn new() -> Self {
        Self {
            ring: Ring::new(),
            lost: 0,
        }
    }

    /// Capacity in records.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Number of pending records.
    pub const fn len(&self) -> usize {
        self.ring.len()
    }

    /// Whether no records are pending.
    pub const fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// Post an event. Returns `true` if it was queued, or `false` if the queue
    /// was full and the event was dropped (and the lost counter bumped).
    pub fn post(&mut self, event: T) -> bool {
        if self.ring.submit(event).is_ok() {
            true
        } else {
            self.lost = self.lost.saturating_add(1);
            false
        }
    }

    /// Take the oldest pending event, if any. Never blocks.
    pub fn poll(&mut self) -> Option<T> {
        self.ring.reap()
    }

    /// Drain up to `out.len()` events into `out`, returning how many were
    /// written. Never blocks.
    pub fn drain_into(&mut self, out: &mut [T]) -> usize {
        let mut n = 0;
        while n < out.len() {
            match self.ring.reap() {
                Some(event) => {
                    out[n] = event;
                    n += 1;
                }
                None => break,
            }
        }
        n
    }

    /// Number of events dropped due to overflow since the last [`clear_lost`].
    ///
    /// [`clear_lost`]: EventQueue::clear_lost
    pub const fn lost(&self) -> u64 {
        self.lost
    }

    /// Read and reset the lost counter.
    pub fn clear_lost(&mut self) -> u64 {
        core::mem::replace(&mut self.lost, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_and_poll_in_order() {
        let mut q: EventQueue<u32, 4> = EventQueue::new();
        assert!(q.is_empty());
        assert!(q.post(1));
        assert!(q.post(2));
        assert_eq!(q.len(), 2);
        assert_eq!(q.poll(), Some(1));
        assert_eq!(q.poll(), Some(2));
        assert_eq!(q.poll(), None);
    }

    #[test]
    fn overflow_drops_and_counts() {
        let mut q: EventQueue<u8, 2> = EventQueue::new();
        assert!(q.post(1));
        assert!(q.post(2));
        assert!(!q.post(3)); // full -> dropped
        assert!(!q.post(4)); // full -> dropped
        assert_eq!(q.lost(), 2);
        // The stream itself is intact: the first two survive, in order.
        assert_eq!(q.poll(), Some(1));
        assert_eq!(q.poll(), Some(2));
        // Lost counter is read-and-reset.
        assert_eq!(q.clear_lost(), 2);
        assert_eq!(q.lost(), 0);
    }

    #[test]
    fn drain_into_respects_slice_length() {
        let mut q: EventQueue<u32, 8> = EventQueue::new();
        for i in 0..5 {
            assert!(q.post(i));
        }
        let mut buf = [0u32; 3];
        assert_eq!(q.drain_into(&mut buf), 3); // limited by buffer
        assert_eq!(buf, [0, 1, 2]);
        let mut rest = [0u32; 8];
        assert_eq!(q.drain_into(&mut rest), 2); // limited by remaining events
        assert_eq!(&rest[..2], &[3, 4]);
        assert!(q.is_empty());
    }
}
