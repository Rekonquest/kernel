//! Capability-ring — zero-copy ownership transfer across protection domains.
//!
//! The microkernel superpower, as a primitive. When drivers live in userspace,
//! the whole game is handing a resource from one domain to another *without
//! copying its bytes* — pass the capability, not the payload. dma-buf/PRIME, an
//! io_uring SQE, a Wayland buffer, a Redox scheme fd handed between processes:
//! all the same move.
//!
//! [`CapabilityRing`] is that move, composed from [`Ring`] (the transfer
//! channel) and [`SeqCounter`] (provenance). It carries values of type `T` —
//! whatever names a resource in a domain-neutral way (a physical frame, a
//! dma-buf id, a scheme fd). Per-domain *naming* is the orchestrator's
//! [`HandleTable`]: the sender [`remove`]s the resource from its own table
//! (which marks its old handle stale) and sends it; the receiver installs the
//! [`Grant`] into *its* table and gets a fresh local handle.
//!
//! Two invariants come for free from the type system — themselves a real wall
//! we operate inside rather than fight:
//!
//! - **Revocation is move.** [`send`] takes the resource by value, so the
//!   sender *cannot* touch it afterward — a compile error, not a runtime check.
//! - **A received capability cannot be lost silently.** [`Grant`] is
//!   `#[must_use]`: drop it without [`install`]ing and the compiler warns.
//!
//! [`Ring`]: crate::ring::Ring
//! [`SeqCounter`]: crate::fence::SeqCounter
//! [`HandleTable`]: crate::handle::HandleTable
//! [`remove`]: crate::handle::HandleTable::remove
//! [`send`]: CapabilityRing::send
//! [`install`]: Grant::install

use crate::{
    fence::SeqCounter,
    ring::{Full, Ring},
};

/// A capability in flight: the transferred resource plus a monotonic grant id
/// for provenance. Constructed only by [`CapabilityRing::send`] and consumed
/// only by [`install`](Grant::install) — so it cannot be forged or duplicated.
#[derive(Debug)]
#[must_use = "a received capability must be installed or it is lost"]
pub struct Grant<T> {
    id: u64,
    resource: T,
}

impl<T> Grant<T> {
    /// The monotonic id this capability was sent with (for tracing/audit).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Consume the grant, yielding the transferred resource. Exactly-once by
    /// move: a grant installs once and cannot be duplicated.
    pub fn install(self) -> T {
        self.resource
    }
}

/// A one-way ownership-transfer channel of `N` capabilities of type `T`.
#[derive(Debug)]
pub struct CapabilityRing<T, const N: usize> {
    ring: Ring<Grant<T>, N>,
    seq: SeqCounter,
    sent: u64,
    received: u64,
}

impl<T, const N: usize> Default for CapabilityRing<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> CapabilityRing<T, N> {
    /// Create an empty channel.
    pub const fn new() -> Self {
        Self {
            ring: Ring::new(),
            seq: SeqCounter::new(),
            sent: 0,
            received: 0,
        }
    }

    /// Transfer ownership of `resource` into the channel. The caller gives it up
    /// by move — using it after this call is a compile error. Returns the grant
    /// id on success, or hands the resource back (via [`Full`]) if the channel
    /// is full, so the caller keeps ownership and can retry.
    pub fn send(&mut self, resource: T) -> Result<u64, Full<T>> {
        let id = self.seq.next_point();
        match self.ring.submit(Grant { id, resource }) {
            Ok(()) => {
                self.sent += 1;
                Ok(id)
            }
            // Unwrap the grant so the caller gets their resource back, not a Grant.
            Err(Full(grant)) => Err(Full(grant.resource)),
        }
    }

    /// Receive the next transferred capability, if any. Ownership passes to the
    /// caller, who must [`install`](Grant::install) the grant to obtain the
    /// resource.
    pub fn recv(&mut self) -> Option<Grant<T>> {
        let grant = self.ring.reap();
        if grant.is_some() {
            self.received += 1;
        }
        grant
    }

    /// Number of capabilities currently in flight (sent but not yet received).
    pub const fn in_flight(&self) -> usize {
        self.ring.len()
    }

    /// Total capabilities ever sent.
    pub const fn sent(&self) -> u64 {
        self.sent
    }

    /// Total capabilities ever received.
    pub const fn received(&self) -> u64 {
        self.received
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HandleTable;

    #[test]
    fn ownership_moves_across_domains() {
        // Two protection domains, each with its own handle namespace.
        let mut domain_a: HandleTable<&str, 4> = HandleTable::new();
        let mut domain_b: HandleTable<&str, 4> = HandleTable::new();
        let mut channel: CapabilityRing<&str, 4> = CapabilityRing::new();

        // A owns a resource locally.
        let h_a = domain_a.alloc("dma-buffer#7").unwrap();
        assert_eq!(domain_a.get(h_a), Some(&"dma-buffer#7"));

        // A relinquishes it (revoke locally) and sends it across.
        let resource = domain_a.remove(h_a).unwrap();
        let grant_id = channel.send(resource).unwrap();
        // A's old handle is now stale — enforced by HandleTable.
        assert_eq!(domain_a.get(h_a), None);

        // B receives the capability and installs it into its own namespace.
        let grant = channel.recv().unwrap();
        assert_eq!(grant.id(), grant_id);
        let resource = grant.install();
        let h_b = domain_b.alloc(resource).unwrap();

        // B can use it; A still cannot. The same value moved — no copy.
        assert_eq!(domain_b.get(h_b), Some(&"dma-buffer#7"));
        assert_eq!(domain_a.get(h_a), None);

        // Exactly-once delivery.
        assert!(channel.recv().is_none());
        assert_eq!(channel.sent(), 1);
        assert_eq!(channel.received(), 1);
    }

    #[test]
    fn full_channel_returns_the_resource() {
        let mut channel: CapabilityRing<u32, 1> = CapabilityRing::new();
        channel.send(1).unwrap();
        assert_eq!(channel.in_flight(), 1);
        match channel.send(2) {
            Err(Full(resource)) => assert_eq!(resource, 2), // got it back
            Ok(_) => panic!("channel should be full"),
        }
    }

    #[test]
    fn grant_ids_are_monotonic_and_carry_the_resource() {
        let mut channel: CapabilityRing<u8, 4> = CapabilityRing::new();
        let id1 = channel.send(10).unwrap();
        let id2 = channel.send(20).unwrap();
        assert!(id2 > id1);

        let g1 = channel.recv().unwrap();
        let g2 = channel.recv().unwrap();
        assert_eq!(g1.id(), id1);
        assert_eq!(g2.id(), id2);
        assert_eq!(g1.install(), 10);
        assert_eq!(g2.install(), 20);
    }
}
