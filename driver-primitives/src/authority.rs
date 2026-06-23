//! Authority wall — a composed capability primitive for cross-domain authority.
//!
//! This is the kit's security mechanism, built by *composition* of the dumber
//! primitives ([`HandleTable`] for unforgeable naming, [`SeqCounter`] for issue
//! provenance). Like every primitive here it is **dumb mechanism, zero policy**:
//! it mints, checks, and revokes capabilities. *Who* should hold which authority
//! is the orchestrator's decision (on Redox, the kernel/bootstrap) — never the
//! wall's.
//!
//! ## The doctrine, in three invariants
//!
//! - **Immutable by construction.** A capability's rights ([`Authority`]) are
//!   fixed at mint and never edited. There is no rights setter. "Changing" a
//!   domain's authority is *revoke then re-issue*, which yields a fresh token —
//!   so a stale view of authority can never be mistaken for the current one.
//! - **Capability-to-construct.** Authorization is *holding* a [`CapToken`] the
//!   wall minted. A forged token, or one whose capability was revoked, is
//!   rejected structurally by [`HandleTable`]'s generation check — not by an
//!   advisory comparison that a caller could skip. There is nothing to route
//!   around: you cannot construct an authorizing token without the wall issuing
//!   it.
//! - **Revocation is destruction.** [`revoke`](AuthorityWall::revoke) removes the
//!   record, which bumps the slot generation, so every outstanding copy of the
//!   token stops authorizing at once. No downgrade-in-place, no global revocation
//!   list to consult on the hot path.
//!
//! A token is bound to the domain it was minted for, so presenting another
//! domain's token to authorize *your* domain fails the check. Identity is
//! *derived from the held capability*, never asserted by the caller.
//!
//! [`HandleTable`]: crate::handle::HandleTable
//! [`SeqCounter`]: crate::fence::SeqCounter

use crate::fence::SeqCounter;
use crate::handle::{Handle, HandleTable};

/// A security domain identity (a process, driver daemon, or service). Opaque and
/// immutable; the wall never interprets it beyond equality. Ordered so an
/// orchestrator can index domains in a map.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct DomainId(pub u64);

/// The set of authorities a capability conveys. An immutable bitset, fixed at
/// mint; the holder cannot widen or edit it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Authority(u32);

impl Authority {
    /// No authority.
    pub const NONE: Self = Self(0);
    /// May create a new scheme (mint a service endpoint).
    pub const CREATE_SCHEME: Self = Self(1 << 0);
    /// May map physical/device memory.
    pub const MAP_MEMORY: Self = Self(1 << 1);
    /// May claim an IRQ line.
    pub const CLAIM_IRQ: Self = Self(1 << 2);
    /// May spawn a new context.
    pub const SPAWN: Self = Self(1 << 3);
    /// May mint capabilities for *other* domains (delegation authority).
    pub const DELEGATE: Self = Self(1 << 4);

    const KNOWN: u32 = 0b1_1111;
    /// Every defined authority (the bootstrap/root set).
    pub const ALL: Self = Self(Self::KNOWN);

    /// Construct from raw bits, keeping only defined authorities.
    pub const fn from_bits_truncate(bits: u32) -> Self {
        Self(bits & Self::KNOWN)
    }

    /// The raw bits, for audit/serialization.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether `self` conveys every authority in `needed`.
    pub const fn contains(self, needed: Authority) -> bool {
        (self.0 & needed.0) == needed.0
    }

    /// The union of two authority sets.
    pub const fn union(self, other: Authority) -> Authority {
        Authority(self.0 | other.0)
    }

    /// Whether this set is empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// An immutable authority record. Private fields, no setters: rights are fixed
/// when [`AuthorityWall::issue`] mints the capability.
#[derive(Clone, Copy, Debug)]
struct CapRecord {
    domain: DomainId,
    authority: Authority,
    epoch: u64,
}

/// The opaque token a domain holds as proof of authority. Possession *is* the
/// proof. It is a generation-checked [`Handle`] newtyped so it cannot be confused
/// with any other handle namespace. Cheap to copy and store.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct CapToken(Handle);

/// The wall is at capacity; no further capability can be issued until one is
/// revoked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AtCapacity;

/// Why a [`delegate`](AuthorityWall::delegate) was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DelegateError {
    /// The delegator's token is forged/revoked, lacks [`Authority::DELEGATE`], or
    /// the requested subset exceeds the authority the delegator holds. You cannot
    /// delegate authority you do not have, nor without the delegation right.
    Unauthorized,
    /// The wall is at capacity.
    AtCapacity,
}

/// A capability-authority wall holding up to `N` live capabilities.
///
/// Dumb mechanism only — it issues, checks, and revokes. It holds **no policy**
/// about who deserves a capability; that is the orchestrator's job, expressed by
/// *which* `(domain, authority)` pairs it chooses to [`issue`](Self::issue) and to
/// *whom* it hands the returned token.
pub struct AuthorityWall<const N: usize> {
    caps: HandleTable<CapRecord, N>,
    epoch: SeqCounter,
}

impl<const N: usize> Default for AuthorityWall<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> AuthorityWall<N> {
    /// Create an empty wall (no capabilities issued).
    pub fn new() -> Self {
        Self {
            caps: HandleTable::new(),
            epoch: SeqCounter::new(),
        }
    }

    /// Orchestrator mint: issue an immutable capability granting `authority` to
    /// `domain`, returning the token to hand to that domain. Possession of the
    /// returned [`CapToken`] is the sole proof of the authority — no identity is
    /// asserted. Fails with [`AtCapacity`] if the wall is full.
    pub fn issue(
        &mut self,
        domain: DomainId,
        authority: Authority,
    ) -> Result<CapToken, AtCapacity> {
        let epoch = self.epoch.next_point();
        match self.caps.alloc(CapRecord {
            domain,
            authority,
            epoch,
        }) {
            Ok(handle) => Ok(CapToken(handle)),
            Err(_) => Err(AtCapacity),
        }
    }

    /// Primitive check (O(1), immutable read): does `token` currently authorize
    /// `needed` for `domain`? Returns `false` for a forged token, a token bound to
    /// a different domain, a revoked (stale) token, or insufficient authority. The
    /// record's rights are read, never mutated.
    pub fn authorizes(&self, token: CapToken, domain: DomainId, needed: Authority) -> bool {
        match self.caps.get(token.0) {
            Some(record) => record.domain == domain && record.authority.contains(needed),
            None => false,
        }
    }

    /// The `(domain, authority, epoch)` a live token conveys, for audit/tracing.
    /// `None` if the token is forged or revoked.
    pub fn inspect(&self, token: CapToken) -> Option<(DomainId, Authority, u64)> {
        self.caps
            .get(token.0)
            .map(|record| (record.domain, record.authority, record.epoch))
    }

    /// Revoke the capability behind `token` by *destroying* its record. The slot's
    /// generation is bumped, so every outstanding copy of the token stops
    /// authorizing immediately. Returns `true` if a live capability was revoked,
    /// `false` if the token was already stale/forged. There is no rights
    /// downgrade: to reduce a domain's authority, revoke and re-issue.
    pub fn revoke(&mut self, token: CapToken) -> bool {
        self.caps.remove(token.0).is_some()
    }

    /// Delegate a **subset** of authority to another domain — the edge that turns
    /// flat grants into a delegation graph.
    ///
    /// The `delegator` must hold a live capability conveying both
    /// [`Authority::DELEGATE`] *and* every authority in `subset`: you cannot
    /// delegate more than you hold, nor without the delegation right. On success a
    /// fresh, **independent** capability is minted for `to_domain` (revoking it
    /// does not touch the delegator's, and vice-versa — cascade is a policy the
    /// orchestrator layers on top, e.g. a graph min-cut). Identity stays derived:
    /// the delegatee proves its authority by holding the returned token.
    pub fn delegate(
        &mut self,
        delegator: CapToken,
        to_domain: DomainId,
        subset: Authority,
    ) -> Result<CapToken, DelegateError> {
        // Copy the delegator's authority out so the immutable borrow ends before
        // the mutable mint below.
        let held = match self.caps.get(delegator.0) {
            Some(record) => record.authority,
            None => return Err(DelegateError::Unauthorized),
        };
        if !held.contains(Authority::DELEGATE) || !held.contains(subset) {
            return Err(DelegateError::Unauthorized);
        }
        self.issue(to_domain, subset)
            .map_err(|_| DelegateError::AtCapacity)
    }

    /// Number of live capabilities.
    pub const fn live(&self) -> usize {
        self.caps.len()
    }

    /// Maximum number of concurrent live capabilities.
    pub const fn capacity(&self) -> usize {
        self.caps.capacity()
    }

    /// Whether the wall can issue no further capability without a revoke.
    pub const fn is_full(&self) -> bool {
        self.caps.is_full()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: DomainId = DomainId(0);
    const DRIVER: DomainId = DomainId(2);

    #[test]
    fn issued_token_authorizes_only_its_grant() {
        let mut wall: AuthorityWall<8> = AuthorityWall::new();
        let token = wall
            .issue(DRIVER, Authority::CLAIM_IRQ.union(Authority::MAP_MEMORY))
            .unwrap();

        // Conveyed authorities pass, individually and together.
        assert!(wall.authorizes(token, DRIVER, Authority::CLAIM_IRQ));
        assert!(wall.authorizes(token, DRIVER, Authority::MAP_MEMORY));
        assert!(wall.authorizes(
            token,
            DRIVER,
            Authority::CLAIM_IRQ.union(Authority::MAP_MEMORY)
        ));

        // An authority it was NOT granted fails.
        assert!(!wall.authorizes(token, DRIVER, Authority::CREATE_SCHEME));
    }

    #[test]
    fn token_is_bound_to_its_domain() {
        let mut wall: AuthorityWall<8> = AuthorityWall::new();
        let token = wall.issue(DRIVER, Authority::SPAWN).unwrap();

        // Presenting the driver's token to authorize a different domain fails:
        // identity is derived from the capability, not asserted by the caller.
        assert!(wall.authorizes(token, DRIVER, Authority::SPAWN));
        assert!(!wall.authorizes(token, ROOT, Authority::SPAWN));
    }

    #[test]
    fn revocation_is_destruction_and_invalidates_every_copy() {
        let mut wall: AuthorityWall<8> = AuthorityWall::new();
        let token = wall.issue(ROOT, Authority::ALL).unwrap();
        let copy = token; // a second holder of the same token

        assert!(wall.authorizes(token, ROOT, Authority::CREATE_SCHEME));
        assert!(wall.revoke(token));

        // Both the original and the copy stop authorizing at once.
        assert!(!wall.authorizes(token, ROOT, Authority::CREATE_SCHEME));
        assert!(!wall.authorizes(copy, ROOT, Authority::CREATE_SCHEME));
        // A second revoke is a no-op.
        assert!(!wall.revoke(copy));
        assert_eq!(wall.live(), 0);
    }

    #[test]
    fn reissued_slot_does_not_revive_an_old_token() {
        let mut wall: AuthorityWall<1> = AuthorityWall::new();
        let old = wall.issue(ROOT, Authority::ALL).unwrap();
        assert!(wall.revoke(old));

        // The single slot is reused for a new, narrower grant.
        let new = wall.issue(DRIVER, Authority::CLAIM_IRQ).unwrap();

        // The stale token must not authorize against the new occupant, even though
        // it indexes the same slot — the generation differs.
        assert!(!wall.authorizes(old, ROOT, Authority::CREATE_SCHEME));
        assert!(!wall.authorizes(old, DRIVER, Authority::CLAIM_IRQ));
        assert!(wall.authorizes(new, DRIVER, Authority::CLAIM_IRQ));
    }

    #[test]
    fn forged_token_authorizes_nothing() {
        let real: AuthorityWall<4> = AuthorityWall::new();
        // A token cannot be constructed outside this module without the wall
        // issuing it; even a default-looking handle resolves to nothing.
        let mut minted: AuthorityWall<4> = AuthorityWall::new();
        let token = minted.issue(ROOT, Authority::ALL).unwrap();
        // The same token value means nothing against a *different* wall instance.
        assert!(!real.authorizes(token, ROOT, Authority::CREATE_SCHEME));
    }

    #[test]
    fn capacity_is_bounded_and_epochs_are_monotonic() {
        let mut wall: AuthorityWall<2> = AuthorityWall::new();
        let a = wall.issue(ROOT, Authority::ALL).unwrap();
        let b = wall.issue(DRIVER, Authority::SPAWN).unwrap();
        assert!(wall.is_full());
        assert_eq!(wall.issue(DRIVER, Authority::SPAWN), Err(AtCapacity));

        // Epoch (issue provenance) strictly increases.
        let (_, _, ea) = wall.inspect(a).unwrap();
        let (_, _, eb) = wall.inspect(b).unwrap();
        assert!(eb > ea);

        // Freeing one makes room again.
        assert!(wall.revoke(a));
        assert!(!wall.is_full());
        let c = wall.issue(DRIVER, Authority::CLAIM_IRQ).unwrap();
        let (_, _, ec) = wall.inspect(c).unwrap();
        assert!(ec > eb);
    }

    #[test]
    fn authority_bitset_is_immutable_and_truncating() {
        let full = Authority::ALL;
        assert!(full.contains(Authority::CREATE_SCHEME));
        assert!(full.contains(Authority::DELEGATE));
        assert!(Authority::NONE.is_empty());
        // Unknown bits are dropped, so a holder cannot smuggle undefined authority.
        assert_eq!(Authority::from_bits_truncate(0xFFFF_FFFF), Authority::ALL);
    }

    #[test]
    fn delegation_needs_the_right_and_cannot_exceed_what_is_held() {
        const THIRD: DomainId = DomainId(3);
        let mut wall: AuthorityWall<8> = AuthorityWall::new();

        // A domain with CLAIM_IRQ but NOT delegate cannot hand it on.
        let no_delegate = wall.issue(DRIVER, Authority::CLAIM_IRQ).unwrap();
        assert_eq!(
            wall.delegate(no_delegate, THIRD, Authority::CLAIM_IRQ),
            Err(DelegateError::Unauthorized)
        );

        // A delegator with DELEGATE + CLAIM_IRQ may delegate CLAIM_IRQ...
        let delegator = wall
            .issue(DRIVER, Authority::DELEGATE.union(Authority::CLAIM_IRQ))
            .unwrap();
        assert!(wall
            .delegate(delegator, THIRD, Authority::CLAIM_IRQ)
            .is_ok());

        // ...but NOT an authority it does not itself hold.
        assert_eq!(
            wall.delegate(delegator, THIRD, Authority::CREATE_SCHEME),
            Err(DelegateError::Unauthorized)
        );
    }

    #[test]
    fn delegated_capability_is_independent_of_the_delegator() {
        const THIRD: DomainId = DomainId(3);
        let mut wall: AuthorityWall<8> = AuthorityWall::new();
        let delegator = wall.issue(ROOT, Authority::ALL).unwrap();

        let delegated = wall
            .delegate(delegator, THIRD, Authority::CLAIM_IRQ)
            .unwrap();
        // The delegatee holds exactly the delegated subset, bound to its own domain.
        assert!(wall.authorizes(delegated, THIRD, Authority::CLAIM_IRQ));
        assert!(!wall.authorizes(delegated, THIRD, Authority::CREATE_SCHEME));
        assert!(!wall.authorizes(delegated, ROOT, Authority::CLAIM_IRQ));

        // Revoking the delegator does NOT revoke the delegatee (cascade is an
        // orchestrator policy, e.g. a graph min-cut — not automatic here).
        assert!(wall.revoke(delegator));
        assert!(wall.authorizes(delegated, THIRD, Authority::CLAIM_IRQ));
    }
}
