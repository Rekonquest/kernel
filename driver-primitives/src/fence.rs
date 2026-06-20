//! Monotonic timeline fence — the dumb substrate under GPU sync objects,
//! command-completion sequence numbers, and any "has progress reached point
//! N?" wait.
//!
//! The pair models the two halves of a submit/complete handshake:
//!
//! - [`SeqCounter`] is the **submission** side: a ticket dispenser handing out
//!   strictly increasing sequence points to tag work with.
//! - [`Fence`] is the **completion** side: a monotonically non-decreasing
//!   high-water mark that only ever moves forward as work finishes.
//!
//! Neither side decides anything about scheduling or ordering policy — they
//! just track and compare integers. A driver that wants "wait until job 42 is
//! done" tags the submission with `seq.next_point()` and later polls
//! `fence.is_passed(42)`.

/// Monotonic ticket dispenser for the submission side. Each [`next_point`] is
/// strictly greater than the last (1, 2, 3, …).
///
/// [`next_point`]: SeqCounter::next_point
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SeqCounter {
    next: u64,
}

impl SeqCounter {
    /// Create a dispenser whose first point will be `1`.
    pub const fn new() -> Self {
        Self { next: 0 }
    }

    /// Dispense the next sequence point.
    pub fn next_point(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// The most recently dispensed point (`0` before the first `next_point`).
    pub const fn peek(&self) -> u64 {
        self.next
    }
}

/// A monotonically non-decreasing completion timeline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fence {
    /// Highest point signalled so far.
    signalled: u64,
}

impl Fence {
    /// Create an unsignalled fence (value `0`).
    pub const fn new() -> Self {
        Self { signalled: 0 }
    }

    /// The highest point reached.
    pub const fn value(&self) -> u64 {
        self.signalled
    }

    /// Signal progress up to `point`. The fence never moves backwards, so a
    /// stale or out-of-order signal (`point <= value()`) is ignored. Returns
    /// `true` if this call advanced the timeline.
    pub fn signal(&mut self, point: u64) -> bool {
        if point > self.signalled {
            self.signalled = point;
            true
        } else {
            false
        }
    }

    /// Whether `point` has been reached (signalled at or beyond it).
    pub const fn is_passed(&self, point: u64) -> bool {
        self.signalled >= point
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_counter_is_strictly_increasing() {
        let mut seq = SeqCounter::new();
        assert_eq!(seq.peek(), 0);
        assert_eq!(seq.next_point(), 1);
        assert_eq!(seq.next_point(), 2);
        assert_eq!(seq.next_point(), 3);
        assert_eq!(seq.peek(), 3);
    }

    #[test]
    fn fence_advances_and_reports_passed() {
        let mut fence = Fence::new();
        assert_eq!(fence.value(), 0);
        assert!(!fence.is_passed(1));
        assert!(fence.signal(5));
        assert!(fence.is_passed(1));
        assert!(fence.is_passed(5));
        assert!(!fence.is_passed(6));
    }

    #[test]
    fn fence_never_moves_backward() {
        let mut fence = Fence::new();
        assert!(fence.signal(10));
        // Stale / out-of-order signals are no-ops.
        assert!(!fence.signal(10));
        assert!(!fence.signal(4));
        assert_eq!(fence.value(), 10);
        // Forward progress still works.
        assert!(fence.signal(11));
        assert_eq!(fence.value(), 11);
    }

    #[test]
    fn out_of_order_completion_still_tracks_high_water_mark() {
        // Work tagged 1,2,3; device completes them out of order. is_passed
        // reflects the furthest contiguous-or-beyond point reached.
        let mut fence = Fence::new();
        assert!(fence.signal(2)); // job 2 done first
        assert!(fence.is_passed(1)); // 1 is implied passed (<= 2)
        assert!(fence.is_passed(2));
        assert!(!fence.is_passed(3));
        assert!(fence.signal(3)); // job 3 done
        assert!(fence.is_passed(3));
    }
}
