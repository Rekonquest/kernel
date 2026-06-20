//! Bounded event ring — the kernel-side instantiation of the driver kit's
//! `Ring` + `EventQueue` primitive (see the `driver-primitives` crate).
//!
//! A producer posts records; a consumer drains them without blocking. When the
//! ring is full the newest record is dropped and a `lost` counter is bumped —
//! exactly like inotify's `IN_Q_OVERFLOW`. The already-queued stream stays
//! intact and in order, the producer never blocks, and the loss is *observable*
//! (via [`EventRing::lost`]) rather than silent, and bounded rather than an
//! unbounded kernel allocation driven by a slow or absent consumer.
//!
//! This is to the kit's `EventQueue` what [`super::super::context::eevdf`] is to
//! the kit's `FairQueue`: the same dumb mechanism, re-derived in-tree so the
//! kernel build stays self-contained. It holds no locks and takes no policy
//! beyond the overflow rule — the *orchestrator* (e.g. a ptrace session) owns
//! the lock and decides when to wake waiters. It is allocation-free (fixed
//! capacity via a const generic) and unit-tested in the clean basis where the
//! logic is cheap to check.

/// A bounded, non-blocking FIFO of up to `N` records of type `T`, with overflow
/// accounting. Allocation-free and lock-free; the caller supplies any needed
/// synchronization.
#[derive(Debug)]
pub struct EventRing<T, const N: usize> {
    slots: [Option<T>; N],
    /// Index of the oldest occupied slot (next to drain).
    head: usize,
    /// Index of the next free slot (next to post into).
    tail: usize,
    len: usize,
    lost: u64,
}

impl<T, const N: usize> Default for EventRing<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> EventRing<T, N> {
    /// Create an empty ring.
    pub const fn new() -> Self {
        Self {
            slots: [const { None }; N],
            head: 0,
            tail: 0,
            len: 0,
            lost: 0,
        }
    }

    /// Total capacity in records.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Number of pending records.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether no records are pending.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the ring has no free slots.
    pub const fn is_full(&self) -> bool {
        self.len == N
    }

    /// Post a record at the tail. Returns `true` if it was queued, or `false`
    /// if the ring was full and the record was dropped (and [`lost`] bumped).
    ///
    /// [`lost`]: EventRing::lost
    pub fn post(&mut self, item: T) -> bool {
        if self.is_full() {
            self.lost = self.lost.saturating_add(1);
            return false;
        }
        self.slots[self.tail] = Some(item);
        self.tail = self.wrap(self.tail + 1);
        self.len += 1;
        true
    }

    /// Take the oldest record from the head, if any. Never blocks.
    pub fn poll(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }
        let item = self.slots[self.head].take();
        self.head = self.wrap(self.head + 1);
        self.len -= 1;
        item
    }

    /// Drain up to `out.len()` records into `out`, oldest first, returning how
    /// many were written. Never blocks.
    pub fn drain_into(&mut self, out: &mut [T]) -> usize {
        let mut n = 0;
        while n < out.len() {
            match self.poll() {
                Some(item) => {
                    out[n] = item;
                    n += 1;
                }
                None => break,
            }
        }
        n
    }

    /// Number of records dropped due to overflow since the last
    /// [`clear_lost`](EventRing::clear_lost).
    pub const fn lost(&self) -> u64 {
        self.lost
    }

    /// Read and reset the lost counter.
    pub fn clear_lost(&mut self) -> u64 {
        core::mem::replace(&mut self.lost, 0)
    }

    /// Advance an index by one slot, wrapping at capacity. Only reached after a
    /// full/empty guard, so never with `N == 0`.
    const fn wrap(&self, index: usize) -> usize {
        if index >= N {
            index - N
        } else {
            index
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_and_drain_preserve_fifo_order() {
        let mut ring: EventRing<u32, 4> = EventRing::new();
        assert!(ring.is_empty());
        assert!(ring.post(1));
        assert!(ring.post(2));
        assert!(ring.post(3));
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.poll(), Some(1));
        assert_eq!(ring.poll(), Some(2));
        assert_eq!(ring.poll(), Some(3));
        assert_eq!(ring.poll(), None);
        assert!(ring.is_empty());
    }

    #[test]
    fn overflow_drops_newest_and_counts_without_corrupting_the_stream() {
        let mut ring: EventRing<u8, 2> = EventRing::new();
        assert!(ring.post(1));
        assert!(ring.post(2));
        assert!(ring.is_full());
        assert!(!ring.post(3)); // full -> dropped + counted
        assert!(!ring.post(4)); // full -> dropped + counted
        assert_eq!(ring.lost(), 2);
        // The already-queued stream survives intact and in order.
        assert_eq!(ring.poll(), Some(1));
        assert_eq!(ring.poll(), Some(2));
        assert_eq!(ring.poll(), None);
        // Lost counter is read-and-reset.
        assert_eq!(ring.clear_lost(), 2);
        assert_eq!(ring.lost(), 0);
    }

    #[test]
    fn drain_into_is_limited_by_both_slice_and_contents() {
        let mut ring: EventRing<u32, 8> = EventRing::new();
        for i in 0..5 {
            assert!(ring.post(i));
        }
        let mut buf = [0u32; 3];
        assert_eq!(ring.drain_into(&mut buf), 3); // limited by buffer
        assert_eq!(buf, [0, 1, 2]);
        let mut rest = [0u32; 8];
        assert_eq!(ring.drain_into(&mut rest), 2); // limited by remaining
        assert_eq!(&rest[..2], &[3, 4]);
        assert!(ring.is_empty());
    }

    #[test]
    fn post_after_drain_reuses_slots_across_the_wrap_boundary() {
        let mut ring: EventRing<usize, 3> = EventRing::new();
        for i in 0..100 {
            assert!(ring.post(i));
            assert_eq!(ring.poll(), Some(i));
            assert!(ring.is_empty());
        }
        // Refill across the wrap point; nothing is lost.
        assert!(ring.post(1));
        assert!(ring.post(2));
        assert!(ring.post(3));
        assert!(ring.is_full());
        assert_eq!(ring.lost(), 0);
        assert_eq!(ring.poll(), Some(1));
        assert_eq!(ring.poll(), Some(2));
        assert_eq!(ring.poll(), Some(3));
    }

    #[test]
    fn empty_to_nonempty_edge_is_observable_for_edge_triggered_notify() {
        // The ptrace orchestrator notifies the tracer only on the 0 -> 1 edge;
        // it reads `is_empty()` before posting. Confirm that reads correctly.
        let mut ring: EventRing<u8, 4> = EventRing::new();
        let was_empty = ring.is_empty();
        let queued = ring.post(7);
        assert!(was_empty && queued); // first event -> notify
        let was_empty = ring.is_empty();
        let queued = ring.post(8);
        assert!(!(was_empty && queued)); // second event -> no notify
    }
}
