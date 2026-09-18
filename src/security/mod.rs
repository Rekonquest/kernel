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
//!   rejected structurally by the table's generation check.
//! - **Revocation is destruction** (a generation bump), not a downgrade-in-place.
//!
//! Identity is **structural**: a domain is named by the kernel from the acting
//! context (its process id), never asserted by the caller.
//!
//! ## Enforcement: scheme creation
//!
//! [`authorize_scheme_create`] is the live gate for `SchemeList::kdup`
//! (`scheme::mod`). It is **behaviour-preserving** — a `uid == 0` caller is
//! granted `CREATE_SCHEME` on first use, exactly the legacy policy — but the
//! authority is now a *recorded, revocable capability*: once a domain is known to
//! the wall, a [`revoke`](AuthorityWall::revoke) sticks, so a compromised root
//! domain's scheme-creation power can be withdrawn at runtime, which the bare
//! `uid == 0` check could never do. [`on_context_exit`] destroys a dying domain's
//! capability so a reused pid starts from a clean slate.

pub mod taint;

use alloc::collections::BTreeMap;

use driver_primitives::{Authority, AuthorityWall, CapToken, DomainId};
use spin::{Mutex, Once};

/// Maximum number of concurrent live authority capabilities, kernel-wide.
///
/// Kept small on purpose: the kit's `HandleTable` is a fixed-capacity array, and
/// the wall is constructed during early boot on the kernel stack — a large array
/// here overflows that stack (a double fault in `kmain`). A microkernel has only
/// a modest number of *concurrently privileged* domains, and `on_context_exit`
/// frees a domain's slot as soon as it dies, so live usage stays well under this.
const WALL_CAPACITY: usize = 256;

/// The bootstrap/root domain.
const ROOT_DOMAIN: DomainId = DomainId(0);

/// The orchestrator's mutable security state: the capability wall (mechanism)
/// plus a domain index. A domain maps to one of three states, and the three are
/// kept distinct — conflating them is how the gate was unsound before:
///   - `Some(token)` — granted; holds the named capability;
///   - `None`        — a **tombstone**: authority was explicitly revoked and must
///                     not be re-granted while the domain stays alive;
///   - absent        — never seen (first contact).
struct SecurityState {
    wall: AuthorityWall<WALL_CAPACITY>,
    domains: BTreeMap<DomainId, Option<CapToken>>,
}

impl SecurityState {
    fn new() -> Self {
        Self {
            wall: AuthorityWall::new(),
            domains: BTreeMap::new(),
        }
    }

    /// Grant `domain` exactly `authority`, revoking any capability it previously
    /// held and clearing any tombstone.
    fn grant(&mut self, domain: DomainId, authority: Authority) {
        if let Some(Some(old)) = self.domains.insert(domain, None) {
            self.wall.revoke(old);
        }
        if let Ok(token) = self.wall.issue(domain, authority) {
            self.domains.insert(domain, Some(token));
        }
    }

    fn authorizes(&self, domain: DomainId, needed: Authority) -> bool {
        match self.domains.get(&domain) {
            Some(Some(token)) => self.wall.authorizes(*token, domain, needed),
            _ => false,
        }
    }

    /// Operator withdrawal of a **live** domain's authority. Leaves a tombstone so
    /// the withdrawal *sticks*: a later gate call for the same live domain is
    /// denied, not silently re-granted.
    fn revoke(&mut self, domain: DomainId) {
        match self.domains.get_mut(&domain) {
            Some(slot) => {
                if let Some(token) = slot.take() {
                    self.wall.revoke(token);
                }
            }
            None => {
                self.domains.insert(domain, None);
            }
        }
    }

    /// Forget a **dead** domain entirely (its pid is gone), so a process that later
    /// reuses the pid starts from first contact rather than inheriting authority or
    /// a tombstone.
    fn forget(&mut self, domain: DomainId) {
        if let Some(Some(token)) = self.domains.remove(&domain) {
            self.wall.revoke(token);
        }
    }

    /// The scheme-creation gate. The legacy policy — only `uid == 0` may create a
    /// scheme — is re-checked on **every** call, so a pid reused by a non-root
    /// process cannot inherit a prior grant. First contact at uid 0 registers the
    /// capability; a tombstoned (revoked) domain stays denied even at uid 0.
    fn authorize_scheme_create(&mut self, domain: DomainId, uid: u32) -> bool {
        if uid != 0 {
            return false;
        }
        match self.domains.get(&domain) {
            None => self.grant(domain, Authority::CREATE_SCHEME), // first contact
            Some(None) => return false,                           // tombstoned: stays revoked
            Some(Some(_)) => {}                                   // already granted
        }
        self.authorizes(domain, Authority::CREATE_SCHEME)
    }
}

/// The single kernel security state, initialized once at boot by [`init`].
static STATE: Once<Mutex<SecurityState>> = Once::new();

/// Initialize the security wall and grant the root domain its full authority.
/// Called once from `kmain`, before the bootstrap context is spawned.
pub fn init() {
    let state = STATE.call_once(|| Mutex::new(SecurityState::new()));
    state.lock().grant(ROOT_DOMAIN, Authority::ALL);
    sense_init();
    taint::init();
}

/// Authorize scheme creation for the calling context (named structurally by
/// `pid`). The legacy `uid == 0` policy is preserved and re-checked every call,
/// but mediated through the revocable capability wall. Before the wall is
/// initialized the legacy policy applies directly.
#[must_use]
pub fn authorize_scheme_create(pid: usize, uid: u32) -> bool {
    let domain = DomainId(pid as u64);
    match STATE.get() {
        Some(state) => state.lock().authorize_scheme_create(domain, uid),
        None => uid == 0,
    }
}

/// Withdraw a live domain's authority and tombstone it so the withdrawal sticks.
/// The control-plane executor for a quarantine decision.
#[allow(dead_code)] // operator/quarantine entry point (wired incrementally)
pub fn revoke_domain(pid: usize) {
    if let Some(state) = STATE.get() {
        state.lock().revoke(DomainId(pid as u64));
    }
}

/// Forget a dying context's domain so its authority cannot be inherited by a later
/// reuse of the same pid. Called from the context exit path.
pub fn on_context_exit(pid: usize) {
    if let Some(state) = STATE.get() {
        state.lock().forget(DomainId(pid as u64));
    }
    sense_forget(pid);
    taint::on_context_exit(pid as u64);
}

// ----- behavior wall (the sense field): what domains DO ------------------
//
// The AuthorityWall above gates CONSTRUCTION (who may create schemes).
// The sense field gates BEHAVIOR (what a running domain is doing): a
// vibration `(pid, class, target)` is recorded at the fs seams where
// a CallerCtx is already built (zero extra locking), the field learns
// the normal conversation map during boot, and post-freeze a domain
// whose behavior leaves the map accumulates foreign budget until
// quarantined — at which point its construction authority is revoked
// through the SAME tombstone path as an operator withdrawal: the two
// walls meet. Revocation is one-way from behavior to construction;
// the wall never grants, only withdraws.

static SENSE: Once<Mutex<driver_primitives::sense::SenseField>> = Once::new();

fn sense() -> Option<&'static Mutex<driver_primitives::sense::SenseField>> {
    SENSE.get()
}

/// Initialize the behavior wall (called from `init`, alongside the AuthorityWall).
fn sense_init() {
    SENSE.call_once(|| Mutex::new(driver_primitives::sense::SenseField::new()));
}

/// Record one vibration at a seam where the caller's pid is already known.
/// `class` distinguishes syscall shapes (e.g. 1 = open, 2 = dup/scheme).
#[allow(dead_code)] // wired incrementally; the scheme gate is first
pub fn sense_record(pid: usize, class: u64, target: u64) {
    if let Some(field) = sense() {
        let mut field = field.lock();
        field.record(pid as u64, class, target, 1.0);
        // Behavior -> construction coupling: a quarantine verdict is
        // executed as a capability revocation (and tombstone), so a
        // condemned domain cannot construct anything new even at
        // uid 0 — the same withdrawal an operator would perform.
        while let Some(condemned) = field.take_intrusion() {
            if let Some(state) = STATE.get() {
                state.lock().revoke(DomainId(condemned as usize as u64));
            }
        }
    }
}

/// Forget a dying context's tracking so a pid reuse starts clean.
pub fn sense_forget(pid: usize) {
    if let Some(field) = sense() {
        field.lock().forget(pid as u64);
    }
}

/// Whether a domain's BEHAVIOR is currently condemned (sustained
/// foreign activity). Read-only; the revocation coupling happens in
/// [`sense_record`].
#[allow(dead_code)]
pub fn sense_quarantined(pid: usize) -> bool {
    sense().is_some_and(|field| field.lock().quarantined(pid as u64))
}
