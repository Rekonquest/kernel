//! Completion-native reactor — the kit assembled into a subsystem.
//!
//! Where the other modules are parts, this is the machine: an io_uring-shaped
//! engine for *any* contended, asynchronous resource. It is built entirely by
//! composing the kit, which is the whole point — the primitives were dumb so
//! that the interesting behaviour lives here, in how they are wired:
//!
//! - [`FairQueue`] decides **which client** is served next (weighted fairness).
//! - a pending area + [`HandleTable`] track each op across its lifecycle, and
//!   the handle's generation counter makes a **stale or duplicate completion**
//!   a rejected no-op rather than a corruption.
//! - [`EventQueue`] carries the completions back out (with overflow counted).
//! - [`SeqCounter`]/[`Fence`] tag submissions and mark completion progress.
//!
//! The cycle is **submit → dispatch → complete → reap**: a client submits an
//! op, the reactor dispatches the fairly-chosen op to an executor, the executor
//! reports a result, and the caller reaps completions. The reactor moves no
//! data and performs no I/O itself — it is mechanism; the executor and the
//! meaning of an "op" are the orchestrator's.
//!
//! [`FairQueue`]: crate::fairqueue::FairQueue
//! [`HandleTable`]: crate::handle::HandleTable
//! [`EventQueue`]: crate::event::EventQueue
//! [`SeqCounter`]: crate::fence::SeqCounter
//! [`Fence`]: crate::fence::Fence

use crate::{
    event::EventQueue,
    fairqueue::FairQueue,
    fence::{Fence, SeqCounter},
    handle::{Handle, HandleTable},
};

/// Identifier handed back from [`Reactor::submit`], correlating to the
/// [`Completion::seq`] that will eventually appear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubmissionId(pub u64);

/// A dispatched op's ticket: what the executor hands back to
/// [`Reactor::complete`]. Opaque; carries the in-flight handle and sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket {
    handle: Handle,
    seq: u64,
}

impl Ticket {
    /// The submission sequence this ticket completes.
    pub const fn seq(self) -> u64 {
        self.seq
    }
}

/// A completion (CQE): the sequence that finished and its result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion<Res> {
    pub seq: u64,
    pub result: Res,
}

struct Pending<Op> {
    client: usize,
    seq: u64,
    op: Op,
}

/// A fair, completion-tracking reactor over `N` in-flight ops and `C` clients.
pub struct Reactor<Op, Res, const N: usize, const C: usize> {
    /// Submitted, not yet dispatched.
    pending: [Option<Pending<Op>>; N],
    /// Dispatched, not yet completed: ticket handle -> sequence.
    inflight: HandleTable<u64, N>,
    /// Completed, not yet reaped.
    cq: EventQueue<Completion<Res>, N>,
    /// Per-client weighted fairness.
    fair: FairQueue<C>,
    seq: SeqCounter,
    /// Latest completed sequence (a progress watermark, not an ordering barrier:
    /// completions may arrive out of order).
    fence: Fence,
}

impl<Op, Res, const N: usize, const C: usize> Default for Reactor<Op, Res, N, C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Op, Res, const N: usize, const C: usize> Reactor<Op, Res, N, C> {
    /// Create an empty reactor.
    pub fn new() -> Self {
        Self {
            pending: [const { None }; N],
            inflight: HandleTable::new(),
            cq: EventQueue::new(),
            fair: FairQueue::new(),
            seq: SeqCounter::new(),
            fence: Fence::new(),
        }
    }

    /// Register client `id` with a fairness `weight` (clamped to `>= 1`). Without
    /// this a client still works but is admitted at the default weight of 1.
    pub fn register_client(&mut self, id: usize, weight: u64) {
        self.fair.activate(id, weight);
    }

    /// Submit `op` on behalf of `client`. Returns a [`SubmissionId`] to match the
    /// eventual completion, or hands `op` back if the pending area is full
    /// (back-pressure — drain by dispatching).
    pub fn submit(&mut self, client: usize, op: Op) -> Result<SubmissionId, Op> {
        let Some(slot) = self.pending.iter().position(|p| p.is_none()) else {
            return Err(op);
        };
        // Auto-admit an unregistered client at the default weight.
        if self.fair.vtime_of(client).is_none() {
            self.fair.activate(client, 1);
        }
        let seq = self.seq.next_point();
        self.pending[slot] = Some(Pending { client, seq, op });
        self.fair.set_backlogged(client, true);
        Ok(SubmissionId(seq))
    }

    /// Dispatch the next op: pick the fairly-chosen backlogged client, take its
    /// oldest pending op, and move it in-flight. Returns the [`Ticket`] and the
    /// op for the executor to perform, or `None` if nothing can be dispatched
    /// (no work, or the in-flight table is full — reap/complete to drain).
    pub fn dispatch(&mut self) -> Option<(Ticket, Op)> {
        if self.inflight.is_full() {
            return None;
        }
        let client = self.fair.pick()?;

        // Oldest (lowest seq) pending op for the chosen client.
        let slot = {
            let mut best: Option<(usize, u64)> = None;
            for (i, p) in self.pending.iter().enumerate() {
                if let Some(pending) = p
                    && pending.client == client
                {
                    match best {
                        Some((_, best_seq)) if pending.seq >= best_seq => {}
                        _ => best = Some((i, pending.seq)),
                    }
                }
            }
            best.map(|(i, _)| i)?
        };

        // Reserve the in-flight slot before taking the op, so a failure never
        // loses work. Capacity matches `pending`, and we checked `is_full`.
        let seq = self.pending[slot].as_ref()?.seq;
        let Ok(handle) = self.inflight.alloc(seq) else {
            return None;
        };
        let op = self.pending[slot].take()?.op;

        // Keep the client's backlog flag honest, then charge it for the turn.
        let more = self
            .pending
            .iter()
            .any(|p| p.as_ref().is_some_and(|pp| pp.client == client));
        if !more {
            self.fair.set_backlogged(client, false);
        }
        self.fair.charge(client, 1);

        Some((Ticket { handle, seq }, op))
    }

    /// Report a result for a dispatched op. Returns `false` if the ticket is
    /// stale or a duplicate (already completed) — rejected via the handle's
    /// generation, never double-counted.
    pub fn complete(&mut self, ticket: Ticket, result: Res) -> bool {
        if self.inflight.remove(ticket.handle).is_none() {
            return false;
        }
        // If the CQ is full the completion is counted as lost, not dropped silently.
        let _ = self.cq.post(Completion {
            seq: ticket.seq,
            result,
        });
        self.fence.signal(ticket.seq);
        true
    }

    /// Reap the next completion, if any. Never blocks.
    pub fn reap(&mut self) -> Option<Completion<Res>> {
        self.cq.poll()
    }

    /// Number of submitted-but-not-dispatched ops.
    pub fn pending_len(&self) -> usize {
        self.pending.iter().filter(|p| p.is_some()).count()
    }

    /// Number of dispatched-but-not-completed ops.
    pub fn in_flight(&self) -> usize {
        self.inflight.len()
    }

    /// The latest completed sequence (a progress watermark).
    pub const fn completed_through(&self) -> u64 {
        self.fence.value()
    }

    /// Completions dropped because the completion queue overflowed.
    pub const fn lost_completions(&self) -> u64 {
        self.cq.lost()
    }

    /// Whether there is no outstanding work at all.
    pub fn is_idle(&self) -> bool {
        self.pending_len() == 0 && self.in_flight() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_dispatch_complete_reap_roundtrip() {
        let mut r: Reactor<&str, u32, 4, 2> = Reactor::new();
        let id = r.submit(0, "read-block").unwrap();
        assert_eq!(r.pending_len(), 1);

        let (ticket, op) = r.dispatch().unwrap();
        assert_eq!(op, "read-block");
        assert_eq!(ticket.seq(), id.0);
        assert_eq!(r.in_flight(), 1);
        assert_eq!(r.pending_len(), 0);

        assert!(r.complete(ticket, 512));
        assert_eq!(r.in_flight(), 0);

        let cqe = r.reap().unwrap();
        assert_eq!(cqe.seq, id.0);
        assert_eq!(cqe.result, 512);
        assert!(r.is_idle());
    }

    #[test]
    fn duplicate_or_stale_completion_is_rejected() {
        let mut r: Reactor<(), (), 4, 1> = Reactor::new();
        r.submit(0, ()).unwrap();
        let (ticket, ()) = r.dispatch().unwrap();
        assert!(r.complete(ticket, ())); // first completion: accepted
        assert!(!r.complete(ticket, ())); // duplicate: rejected, not double-counted
        assert_eq!(r.reap().map(|c| c.seq), Some(ticket.seq()));
        assert!(r.reap().is_none()); // only one completion was ever posted
    }

    #[test]
    #[allow(clippy::needless_range_loop)] // `c` is both the index and the client id
    fn dispatch_order_is_fair_across_clients() {
        // Two clients, weights 1 and 3, both kept continuously backlogged. The
        // reactor should serve the weight-3 client ~3x as often — the scheduler
        // fairness property, now arbitrating device ops instead of CPU time.
        let mut r: Reactor<usize, (), 8, 2> = Reactor::new();
        r.register_client(0, 1);
        r.register_client(1, 3);

        let mut outstanding = [0usize; 2];
        let mut served = [0u32; 2];
        for _ in 0..4000 {
            for c in 0..2 {
                if outstanding[c] == 0 && r.submit(c, c).is_ok() {
                    outstanding[c] += 1;
                }
            }
            if let Some((ticket, client)) = r.dispatch() {
                outstanding[client] -= 1;
                served[client] += 1;
                r.complete(ticket, ());
                let _ = r.reap();
            }
        }

        assert_eq!(served[0] + served[1], 4000);
        assert!((980..=1020).contains(&served[0]), "served = {served:?}");
        assert!((2980..=3020).contains(&served[1]), "served = {served:?}");
    }

    #[test]
    fn pending_area_applies_backpressure() {
        let mut r: Reactor<u8, (), 2, 1> = Reactor::new();
        r.submit(0, 1).unwrap();
        r.submit(0, 2).unwrap();
        assert_eq!(r.pending_len(), 2);
        // Pending area full -> the op is handed back.
        assert_eq!(r.submit(0, 3), Err(3));
        // Draining one makes room again.
        let (t, _) = r.dispatch().unwrap();
        r.submit(0, 4).unwrap();
        assert!(r.complete(t, ()));
    }
}
