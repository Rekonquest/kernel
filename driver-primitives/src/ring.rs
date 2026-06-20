//! Bounded descriptor ring — the dumb substrate under every driver's
//! submit/reap path.
//!
//! A driver typically wires *two* of these: one submission ring
//! (driver → device) and one completion ring (device → driver), exactly like
//! virtio's avail/used pair or NVMe's submission/completion queues. The ring
//! itself makes no policy decisions — it is a fixed-capacity FIFO of
//! descriptors and nothing more. Ordering across rings, back-pressure policy,
//! and what a "descriptor" means are all the orchestrator's job.

/// Returned by [`Ring::submit`] when the ring is full. Carries the rejected
/// item back to the caller so nothing is silently dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Full<T>(pub T);

/// A fixed-capacity FIFO ring of `N` descriptors of type `T`.
#[derive(Debug)]
pub struct Ring<T, const N: usize> {
    slots: [Option<T>; N],
    /// Index of the oldest occupied slot (next to reap).
    head: usize,
    /// Index of the next free slot (next to submit into).
    tail: usize,
    len: usize,
}

impl<T, const N: usize> Default for Ring<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Ring<T, N> {
    /// Create an empty ring.
    pub const fn new() -> Self {
        Self {
            slots: [const { None }; N],
            head: 0,
            tail: 0,
            len: 0,
        }
    }

    /// Total number of slots.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Number of occupied slots.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the ring holds no descriptors.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the ring has no free slots.
    pub const fn is_full(&self) -> bool {
        self.len == N
    }

    /// Submit (enqueue) a descriptor at the tail. On a full ring the item is
    /// handed back unchanged via [`Full`].
    pub fn submit(&mut self, item: T) -> Result<(), Full<T>> {
        if self.is_full() {
            return Err(Full(item));
        }
        self.slots[self.tail] = Some(item);
        self.tail = self.wrap(self.tail + 1);
        self.len += 1;
        Ok(())
    }

    /// Reap (dequeue) the oldest descriptor from the head, if any.
    pub fn reap(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }
        let item = self.slots[self.head].take();
        self.head = self.wrap(self.head + 1);
        self.len -= 1;
        item
    }

    /// Borrow the oldest descriptor without removing it.
    pub fn peek(&self) -> Option<&T> {
        if self.is_empty() {
            None
        } else {
            self.slots[self.head].as_ref()
        }
    }

    /// Advance an index by one slot, wrapping at capacity. Only reached when
    /// `N > 0` (submit/reap guard on full/empty first).
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
    fn fifo_order_is_preserved() {
        let mut ring: Ring<u32, 4> = Ring::new();
        assert!(ring.is_empty());
        ring.submit(1).unwrap();
        ring.submit(2).unwrap();
        ring.submit(3).unwrap();
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.reap(), Some(1));
        assert_eq!(ring.reap(), Some(2));
        assert_eq!(ring.reap(), Some(3));
        assert_eq!(ring.reap(), None);
        assert!(ring.is_empty());
    }

    #[test]
    fn full_returns_the_item() {
        let mut ring: Ring<u8, 2> = Ring::new();
        ring.submit(10).unwrap();
        ring.submit(20).unwrap();
        assert!(ring.is_full());
        assert_eq!(ring.submit(30), Err(Full(30)));
        // Nothing was dropped or corrupted.
        assert_eq!(ring.reap(), Some(10));
        assert_eq!(ring.reap(), Some(20));
    }

    #[test]
    fn wraps_around_many_times() {
        let mut ring: Ring<usize, 3> = Ring::new();
        // Push/pop far more than capacity to exercise index wraparound.
        for i in 0..100 {
            ring.submit(i).unwrap();
            assert_eq!(ring.reap(), Some(i));
            assert!(ring.is_empty());
        }
        // Interleaved fill/drain across the wrap boundary.
        ring.submit(0).unwrap();
        ring.submit(1).unwrap();
        assert_eq!(ring.reap(), Some(0));
        ring.submit(2).unwrap();
        ring.submit(3).unwrap();
        assert!(ring.is_full());
        assert_eq!(ring.reap(), Some(1));
        assert_eq!(ring.reap(), Some(2));
        assert_eq!(ring.reap(), Some(3));
    }

    #[test]
    fn peek_does_not_consume() {
        let mut ring: Ring<u32, 2> = Ring::new();
        assert_eq!(ring.peek(), None);
        ring.submit(42).unwrap();
        assert_eq!(ring.peek(), Some(&42));
        assert_eq!(ring.peek(), Some(&42));
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.reap(), Some(42));
    }

    #[test]
    fn zero_capacity_is_always_full() {
        let mut ring: Ring<u8, 0> = Ring::new();
        assert!(ring.is_full());
        assert!(ring.is_empty());
        assert_eq!(ring.submit(1), Err(Full(1)));
        assert_eq!(ring.reap(), None);
    }
}
