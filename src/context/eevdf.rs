//! Pure EEVDF scheduling policy.
//!
//! This module is the *orchestrator* half of the scheduler: all policy, no
//! mechanism. It contains only the virtual-time arithmetic that decides which
//! context should run next and how virtual time advances. It holds no locks and
//! touches no `Context`, `Arc`, or run queue — it operates on plain [`Entity`]
//! snapshots and integers, in the clean basis where the math is cheap and
//! unit-testable.
//!
//! The mechanism (queue walks, weak-ref upgrades, per-context locking, address
//! space acquisition) lives in [`super::switch`] and *drives* these functions.
//! The seam between the two is deliberately explicit: the caller copies the
//! relevant `Context` fields into an [`Entity`], runs the policy here, and
//! copies the results back. Nothing in this file knows how a context is stored
//! or locked.

use core::cmp::Ordering;

/// Fixed-point scale for virtual-time arithmetic.
pub const SCALE: u128 = 1 << 40;
/// PIT ticks between scheduler invocations (~6.75 ms).
pub const TICK_INTERVAL: u64 = 3;
/// Base time slice handed to a freshly scheduled context, in ticks (~20.25 ms).
pub const BASE_SLICE_TICKS: u64 = TICK_INTERVAL * 3;
/// Nanoseconds per PIT tick (~2.25 ms).
pub const NANOS_PER_TICK: u128 = 2_250_000;

/// Maps a nice-like priority (`0..40`) to an EEVDF weight. A geometric series
/// where `weight[i] ~= weight[i + 1] * 1.25`.
const SCHED_PRIO_TO_WEIGHT: [usize; 40] = [
    88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, 9548, 7620, 6100, 4904,
    3906, 3121, 2501, 1991, 1586, 1277, 1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87,
    70, 56, 45, 36, 29, 23, 18, 15,
];

/// Weight for a given priority. Callers pass `Context::prio`, which is always
/// within range; this mirrors the historical direct-index behaviour.
pub fn weight_of(prio: usize) -> u64 {
    SCHED_PRIO_TO_WEIGHT[prio] as u64
}

/// A plain-data snapshot of a context's EEVDF state. This is the basis the
/// policy operates in: copy the relevant `Context` fields in, run the math,
/// copy the results back. No locks, no kernel types.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entity {
    /// Virtual run time.
    pub vtime: u64,
    /// Virtual deadline.
    pub vd: u64,
    /// Remaining slice (scaled ticks).
    pub rem_slice: u64,
}

/// Convert an elapsed wall-clock duration (ns) into scaled virtual ticks.
pub fn elapsed_ticks(elapsed_nanos: u64) -> u128 {
    elapsed_nanos as u128 * SCALE / NANOS_PER_TICK
}

/// Whether a context that ran for `elapsed_nanos` is treated as having yielded
/// early, i.e. it gave up the CPU well before consuming its tick budget.
pub fn is_yield(elapsed_nanos: u64) -> bool {
    (elapsed_nanos as u128) < (TICK_INTERVAL as u128 * NANOS_PER_TICK) / 2
}

/// A context is *eligible* when its virtual time has not run ahead of the
/// global virtual time `v`.
pub fn eligible(vtime: u64, v: u64) -> bool {
    vtime <= v
}

/// Ordering over scheduling candidates: earliest virtual deadline first, then
/// larger remaining slice. [`Ordering::Less`] means the left candidate is
/// preferred. This is exactly the order the run queue's
/// `(vd, Reverse(rem_slice), id)` key already materialises.
pub fn prefer(a: &Entity, b: &Entity) -> Ordering {
    a.vd.cmp(&b.vd).then(b.rem_slice.cmp(&a.rem_slice))
}

/// Reset a context's slice to a fresh base slice and recompute its virtual
/// deadline from its current virtual time.
pub fn reset_slice(e: &mut Entity, weight: u64) {
    e.rem_slice = BASE_SLICE_TICKS * SCALE as u64;
    e.vd = e.vtime + (BASE_SLICE_TICKS as u128 * SCALE / weight as u128) as u64;
}

/// Charge a context for the time it just ran: advance its virtual time and
/// shrink its remaining slice. If it yielded early it is penalised for the
/// unconsumed remainder. When the slice is exhausted, a fresh slice/deadline is
/// assigned. `v` is the current global virtual time, used as a floor for the
/// context's virtual time.
pub fn charge(e: &mut Entity, weight: u64, elapsed_ticks: u128, did_yield: bool, v: u64) {
    e.rem_slice = e.rem_slice.saturating_sub(elapsed_ticks as u64);
    e.vtime += (elapsed_ticks / weight as u128) as u64;

    if e.vtime < v {
        e.vtime = v;
    }

    if did_yield {
        e.vtime += (e.rem_slice as u128 / weight as u128) as u64;
        e.rem_slice = 0;
    }

    if e.rem_slice == 0 {
        reset_slice(e, weight);
    }
}

/// Bring a previously-idle context back into the run queue: clamp its virtual
/// time up to the global virtual time and give it a fresh slice/deadline.
pub fn activate(e: &mut Entity, weight: u64, v: u64) {
    e.vtime = e.vtime.max(v);
    reset_slice(e, weight);
}

/// How much the global virtual time advances after a slice during which the
/// runnable set had total weight `total_weight`. Returns 0 when nothing is
/// active (avoiding division by zero).
pub fn v_advance(elapsed_ticks: u128, total_weight: u64) -> u64 {
    if total_weight > 0 {
        (elapsed_ticks / total_weight as u128) as u64
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference constants, computed independently of the implementation.
    const W20: u64 = 1024; // weight_of(20)
    const REM_BASE: u64 = BASE_SLICE_TICKS * (SCALE as u64); // 9 << 40
    const SLICE20: u64 = (BASE_SLICE_TICKS as u128 * SCALE / W20 as u128) as u64; // 9 << 30

    #[test]
    fn weights_are_stable() {
        assert_eq!(weight_of(0), 88761);
        assert_eq!(weight_of(20), 1024);
        assert_eq!(weight_of(39), 15);
    }

    #[test]
    fn elapsed_ticks_is_scale_normalised() {
        // One tick's worth of nanoseconds maps to exactly SCALE.
        assert_eq!(elapsed_ticks(NANOS_PER_TICK as u64), SCALE);
        assert_eq!(elapsed_ticks(0), 0);
    }

    #[test]
    fn yield_threshold_is_half_a_tick_interval() {
        let threshold = (TICK_INTERVAL as u128 * NANOS_PER_TICK / 2) as u64;
        assert!(is_yield(threshold - 1));
        assert!(!is_yield(threshold));
    }

    #[test]
    fn eligibility_is_vtime_le_v() {
        assert!(eligible(5, 5));
        assert!(eligible(4, 5));
        assert!(!eligible(6, 5));
    }

    #[test]
    fn prefer_orders_by_deadline_then_slice() {
        let early = Entity { vd: 10, ..Default::default() };
        let late = Entity { vd: 20, ..Default::default() };
        assert_eq!(prefer(&early, &late), Ordering::Less);
        assert_eq!(prefer(&late, &early), Ordering::Greater);

        // Equal deadlines: the larger remaining slice wins (is preferred).
        let big = Entity { vd: 10, rem_slice: 100, ..Default::default() };
        let small = Entity { vd: 10, rem_slice: 50, ..Default::default() };
        assert_eq!(prefer(&big, &small), Ordering::Less);
        assert_eq!(prefer(&small, &big), Ordering::Greater);
        assert_eq!(prefer(&big, &big), Ordering::Equal);
    }

    #[test]
    fn reset_slice_sets_base_slice_and_deadline() {
        let mut e = Entity { vtime: 7, vd: 0, rem_slice: 0 };
        reset_slice(&mut e, W20);
        assert_eq!(e.rem_slice, REM_BASE);
        assert_eq!(e.vd, 7 + SLICE20);
    }

    #[test]
    fn charge_advances_vtime_and_shrinks_slice() {
        let mut e = Entity { vtime: 0, vd: 0, rem_slice: REM_BASE };
        // Run for exactly one tick (SCALE) at weight 1024.
        charge(&mut e, W20, SCALE, false, 0);
        assert_eq!(e.rem_slice, REM_BASE - SCALE as u64);
        assert_eq!(e.vtime, (SCALE / W20 as u128) as u64);
        assert_eq!(e.vd, 0); // not reset; slice not exhausted
    }

    #[test]
    fn charge_resets_when_slice_exhausted() {
        let mut e = Entity { vtime: 0, vd: 0, rem_slice: 1000 };
        charge(&mut e, W20, 2000, false, 0);
        assert_eq!(e.rem_slice, REM_BASE); // saturated to 0, then reset
        assert_eq!(e.vtime, (2000u128 / W20 as u128) as u64);
        assert_eq!(e.vd, e.vtime + SLICE20);
    }

    #[test]
    fn charge_clamps_vtime_to_global_floor() {
        let mut e = Entity { vtime: 0, vd: 0, rem_slice: REM_BASE };
        charge(&mut e, W20, 0, false, 500);
        assert_eq!(e.vtime, 500);
    }

    #[test]
    fn charge_penalises_early_yield() {
        let mut e = Entity { vtime: 0, vd: 0, rem_slice: 5 * W20 };
        charge(&mut e, W20, 0, true, 0);
        // Yield penalty: unconsumed (5*W20) / W20 == 5 added to vtime; slice zeroed then reset.
        assert_eq!(e.vtime, 5);
        assert_eq!(e.rem_slice, REM_BASE);
        assert_eq!(e.vd, 5 + SLICE20);
    }

    #[test]
    fn activate_clamps_vtime_and_refreshes_slice() {
        let mut e = Entity { vtime: 3, vd: 0, rem_slice: 0 };
        activate(&mut e, W20, 10);
        assert_eq!(e.vtime, 10);
        assert_eq!(e.rem_slice, REM_BASE);
        assert_eq!(e.vd, 10 + SLICE20);
    }

    #[test]
    fn v_advance_divides_by_total_weight_and_guards_zero() {
        assert_eq!(v_advance(2048, 2), 1024);
        assert_eq!(v_advance(2048, 0), 0);
    }
}
