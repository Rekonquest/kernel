//! The scheduler run queue — the *mechanism* half of EEVDF.
//!
//! [`super::eevdf`] is the *policy*: the virtual-time arithmetic that decides
//! who runs next. This module is the mechanism that policy drives: the ordered
//! set of runnable contexts plus the two pieces of global bookkeeping the policy
//! reads and updates — the global virtual time `V` and the total weight of the
//! active set.
//!
//! It is the kernel's instantiation of the kit's `FairQueue` primitive (see the
//! `driver-primitives` crate): "among entities contending for one resource,
//! serve the one with the least virtual time, weighted by priority." Here the
//! resource is the CPU and the ordering is EEVDF's — earliest virtual *deadline*
//! first — so the structure is a [`BTreeMap`] keyed by
//! `(vd, Reverse(rem_slice), id)` rather than the kit's flat array, but the role
//! is the same. Encapsulating it behind a small API makes the bookkeeping
//! invariants explicit and unit-testable instead of open-coded across the switch
//! path:
//!
//! - active weight only ever reflects the enqueued/active set (saturating, so a
//!   double-remove can never underflow it), and
//! - global virtual time is advanced through one method, never poked directly.
//!
//! The algorithm is unchanged: keys are `(vd, Reverse(rem_slice), id)` exactly
//! as before, so `BTreeMap` iteration yields the EEVDF order
//! [`super::switch::select_next_context`] depends on. This type is generic over
//! the context handle `C`, holds no locks, and touches no `Context`; the switch
//! path owns the lock and does all per-context work.

use alloc::collections::{btree_map, BTreeMap};
use core::cmp::Reverse;

/// Ordering key for a runnable context: earliest virtual deadline first, then
/// larger remaining slice (via [`Reverse`]), then context id as a tiebreak.
/// This is exactly the order [`super::eevdf::prefer`] defines, materialised as a
/// [`BTreeMap`] key.
pub type RunKey = (u64, Reverse<u64>, u32);

/// What the queue stores per context: the `(vtime, weight)` snapshot the walk
/// needs *without* taking the context lock, plus the handle `C` to the context.
pub type RunValue<C> = (u64, u64, C);

/// The set of runnable contexts ordered for EEVDF selection, together with the
/// global virtual time and total active weight the policy needs.
pub struct RunQueue<C> {
    tree: BTreeMap<RunKey, RunValue<C>>,
    /// Global virtual time `V`. Advanced only via [`RunQueue::advance_v`] /
    /// [`RunQueue::set_v`].
    v: u64,
    /// Sum of the weights of the currently-active contexts.
    total_weight: u64,
}

impl<C> Default for RunQueue<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C> RunQueue<C> {
    /// An empty run queue with virtual time and active weight at zero.
    pub const fn new() -> Self {
        Self {
            tree: BTreeMap::new(),
            v: 0,
            total_weight: 0,
        }
    }

    // --- global virtual time `V` ---

    /// The current global virtual time.
    pub fn v(&self) -> u64 {
        self.v
    }

    /// Set the global virtual time (used when `V` jumps to the earliest
    /// ineligible virtual time during a walk).
    pub fn set_v(&mut self, v: u64) {
        self.v = v;
    }

    /// Advance the global virtual time by `delta` (a slice normalised by active
    /// weight; see [`super::eevdf::v_advance`]).
    pub fn advance_v(&mut self, delta: u64) {
        self.v += delta;
    }

    // --- total active weight ---

    /// Sum of the weights of the active set.
    pub fn total_weight(&self) -> u64 {
        self.total_weight
    }

    /// Add a newly-active context's weight to the active set.
    pub fn add_weight(&mut self, weight: u64) {
        self.total_weight += weight;
    }

    /// Drop a no-longer-active context's weight from the active set. Saturating,
    /// so it can never underflow if a context is accounted out twice.
    pub fn sub_weight(&mut self, weight: u64) {
        self.total_weight = self.total_weight.saturating_sub(weight);
    }

    // --- membership ---

    /// Number of enqueued contexts.
    pub fn len(&self) -> usize {
        self.tree.len()
    }

    /// Whether no contexts are enqueued.
    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    /// Enqueue (or replace) the entry under `key`.
    pub fn insert(&mut self, key: RunKey, value: RunValue<C>) -> Option<RunValue<C>> {
        self.tree.insert(key, value)
    }

    /// Remove the entry under `key`, if present.
    pub fn remove(&mut self, key: &RunKey) -> Option<RunValue<C>> {
        self.tree.remove(key)
    }

    /// Iterate the queue in EEVDF order (earliest virtual deadline first).
    pub fn iter(&self) -> btree_map::Iter<'_, RunKey, RunValue<C>> {
        self.tree.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a key the way the switch path does.
    fn key(vd: u64, rem_slice: u64, id: u32) -> RunKey {
        (vd, Reverse(rem_slice), id)
    }

    #[test]
    fn starts_empty_with_zeroed_bookkeeping() {
        let rq: RunQueue<u32> = RunQueue::new();
        assert!(rq.is_empty());
        assert_eq!(rq.len(), 0);
        assert_eq!(rq.v(), 0);
        assert_eq!(rq.total_weight(), 0);
    }

    #[test]
    fn iterates_in_eevdf_order_deadline_then_larger_slice_then_id() {
        let mut rq: RunQueue<u32> = RunQueue::new();
        // Insert deliberately out of order.
        rq.insert(key(20, 5, 1), (0, 1, 101)); // latest deadline
        rq.insert(key(10, 5, 3), (0, 1, 103)); // same deadline as below, smaller slice
        rq.insert(key(10, 9, 2), (0, 1, 102)); // earliest-tie: larger slice wins
        rq.insert(key(10, 9, 0), (0, 1, 100)); // same vd+slice: smaller id wins

        let order: alloc::vec::Vec<u32> = rq.iter().map(|(_, (_, _, payload))| *payload).collect();
        // vd=10 group first; within it, larger rem_slice (9) before smaller (5),
        // and for equal (vd, rem_slice) the smaller id; then vd=20 last.
        assert_eq!(order, alloc::vec![100, 102, 103, 101]);
    }

    #[test]
    fn insert_remove_track_len_and_return_value() {
        let mut rq: RunQueue<u32> = RunQueue::new();
        assert_eq!(rq.insert(key(1, 1, 0), (7, 2, 42)), None);
        assert_eq!(rq.len(), 1);
        // Replacing the same key returns the old value.
        assert_eq!(rq.insert(key(1, 1, 0), (8, 2, 43)), Some((7, 2, 42)));
        assert_eq!(rq.len(), 1);
        assert_eq!(rq.remove(&key(1, 1, 0)), Some((8, 2, 43)));
        assert!(rq.is_empty());
        assert_eq!(rq.remove(&key(1, 1, 0)), None);
    }

    #[test]
    fn weight_accounting_is_saturating() {
        let mut rq: RunQueue<u32> = RunQueue::new();
        rq.add_weight(100);
        rq.add_weight(50);
        assert_eq!(rq.total_weight(), 150);
        rq.sub_weight(60);
        assert_eq!(rq.total_weight(), 90);
        // Over-subtraction can never wrap below zero.
        rq.sub_weight(1000);
        assert_eq!(rq.total_weight(), 0);
    }

    #[test]
    fn virtual_time_advances_and_can_jump() {
        let mut rq: RunQueue<u32> = RunQueue::new();
        rq.advance_v(10);
        rq.advance_v(5);
        assert_eq!(rq.v(), 15);
        // A walk that finds only ineligible contexts jumps V forward.
        rq.set_v(100);
        assert_eq!(rq.v(), 100);
    }
}
