//! Kernel security wall — the *orchestrator* side of [`AuthorityWall`].
//!
//! `driver_primitives` provides the dumb mechanism: an immutable,
//! capability-to-construct authority table. The kernel is the **orchestrator** —
//! it owns the single wall instance, decides which domain receives which
//! authority, and consults the wall at the seams where a cross-domain operation
//! must be authorized.
//!
//! Doctrine held here:
//!
//! - Authority is an **immutable capability** *held* as a [`CapToken`]; the
//!   bearer cannot widen or edit it.
//! - **Authorization is possession.** A domain's authority is whatever live
//!   capability the orchestrator minted for it; a forged or revoked token is
//!   rejected structurally by the table's generation check, not by an advisory
//!   comparison.
//! - **Revocation is destruction** (a generation bump), not a downgrade-in-place.
//!   Re-granting a domain a different authority *revokes the old capability* and
//!   mints a fresh one, so a stale authority can never be mistaken for current.
//!
//! Identity is **structural**: a domain is named by the kernel from the acting
//! context (its owning process), never asserted by the caller. The domain index
//! below maps that structural identity to the capability the orchestrator issued.
//!
//! This is the capability replacement for the ambient `euid == 0` checks the
//! kernel still carries (see the `// TODO: Remove these from kernel` on the
//! bootstrap context in `startup::kmain`). The authorization facade
//! ([`authorize_domain`]) is consumed by those seams incrementally, each behind
//! its own design trace; flipping a production gate is deferred until the
//! orchestrator grants every spawned domain its authority (capability
//! distribution) and the change is boot-verified.

use alloc::collections::BTreeMap;

use driver_primitives::{Authority, AuthorityWall, CapToken, DomainId};
use spin::{Mutex, Once};

/// Maximum number of concurrent live authority capabilities, kernel-wide.
const WALL_CAPACITY: usize = 1024;

/// The bootstrap/root domain: the first userspace context, which receives the
/// full authority set at boot and is the source from which all other authority
/// is delegated.
pub const ROOT_DOMAIN: DomainId = DomainId(0);

/// The orchestrator's mutable security state: the capability wall (mechanism)
/// plus the domain index mapping a structural domain identity to the capability
/// the orchestrator issued it. One capability per domain; re-granting revokes the
/// previous one.
struct SecurityState {
    wall: AuthorityWall<WALL_CAPACITY>,
    domains: BTreeMap<DomainId, CapToken>,
}

impl SecurityState {
    fn new() -> Self {
        Self {
            wall: AuthorityWall::new(),
            domains: BTreeMap::new(),
        }
    }

    /// Grant `domain` exactly `authority`, replacing (and revoking) any capability
    /// it previously held. Returns the fresh token, or `None` at capacity.
    fn grant(&mut self, domain: DomainId, authority: Authority) -> Option<CapToken> {
        if let Some(old) = self.domains.remove(&domain) {
            self.wall.revoke(old);
        }
        let token = self.wall.issue(domain, authority).ok()?;
        self.domains.insert(domain, token);
        Some(token)
    }

    fn authorizes(&self, domain: DomainId, needed: Authority) -> bool {
        match self.domains.get(&domain) {
            Some(token) => self.wall.authorizes(*token, domain, needed),
            None => false,
        }
    }

    fn revoke(&mut self, domain: DomainId) -> bool {
        match self.domains.remove(&domain) {
            Some(token) => self.wall.revoke(token),
            None => false,
        }
    }
}

/// The single kernel security state, initialized once at boot by [`init`].
static STATE: Once<Mutex<SecurityState>> = Once::new();

/// Initialize the security wall and grant the root domain its full authority.
/// Called once from `kmain`, before the bootstrap context is spawned. Idempotent.
pub fn init() {
    let state = STATE.call_once(|| Mutex::new(SecurityState::new()));
    state
        .lock()
        .grant(ROOT_DOMAIN, Authority::ALL)
        .expect("security: root authority must grant on an empty wall");
}

/// Grant `domain` exactly `authority` (an immutable capability), replacing and
/// revoking any capability it previously held. This is the orchestrator's
/// *distribution* decision — called as contexts are created and as authority is
/// delegated. Returns `false` if the wall is uninitialized or at capacity.
#[allow(dead_code)] // consumed by spawn-time capability distribution (wired incrementally)
pub fn grant_domain(domain: DomainId, authority: Authority) -> bool {
    match STATE.get() {
        Some(state) => state.lock().grant(domain, authority).is_some(),
        None => false,
    }
}

/// Authorize a cross-domain operation: does `domain` currently hold a capability
/// conveying `needed`? Identity is structural (the kernel names `domain` from the
/// acting context). An unknown domain, an insufficient capability, or an
/// uninitialized wall all deny.
#[allow(dead_code)] // consumed by cross-domain authorization seams (wired incrementally)
#[must_use]
pub fn authorize_domain(domain: DomainId, needed: Authority) -> bool {
    match STATE.get() {
        Some(state) => state.lock().authorizes(domain, needed),
        None => false,
    }
}

/// Revoke `domain`'s capability by destroying its record; the generation bump
/// invalidates the token at once. Called when a context dies or its authority is
/// withdrawn. Returns `true` if a live capability was revoked.
#[allow(dead_code)] // consumed by context teardown / quarantine seams (wired incrementally)
pub fn revoke_domain(domain: DomainId) -> bool {
    match STATE.get() {
        Some(state) => state.lock().revoke(domain),
        None => false,
    }
}
