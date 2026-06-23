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
//! - **Authorization is possession.** The kernel asserts no identity — holding a
//!   wall-minted token is the proof, and a forged or revoked token is rejected
//!   structurally by the table's generation check.
//! - **Revocation is destruction** (a generation bump), not a downgrade-in-place.
//!
//! This is the capability replacement for the ambient `euid == 0` checks the
//! kernel still carries (see the `// TODO: Remove these from kernel` on the
//! bootstrap context in `startup::kmain`). The authorization facade
//! ([`authorize`], [`issue`], [`revoke`]) is consumed by those seams
//! incrementally, each behind its own design trace.

use driver_primitives::{Authority, AuthorityWall, CapToken, DomainId};
use spin::{Mutex, Once};

/// Maximum number of concurrent live authority capabilities, kernel-wide.
const WALL_CAPACITY: usize = 1024;

/// The bootstrap/root domain: the first userspace context, which receives the
/// full authority set at boot and is the source from which all other authority
/// is delegated.
pub const ROOT_DOMAIN: DomainId = DomainId(0);

/// The single kernel security wall, initialized once at boot by [`init`].
static WALL: Once<Mutex<AuthorityWall<WALL_CAPACITY>>> = Once::new();

/// The root domain's full-authority token, retained so the bootstrap authority
/// can be presented and delegated.
static ROOT_TOKEN: Once<CapToken> = Once::new();

/// Initialize the security wall and mint the root domain's full-authority
/// capability. Called once from `kmain`, before the bootstrap context is spawned.
/// Idempotent: a second call is a no-op.
pub fn init() {
    let wall = WALL.call_once(|| Mutex::new(AuthorityWall::new()));
    let token = wall
        .lock()
        .issue(ROOT_DOMAIN, Authority::ALL)
        .expect("security: root authority must issue on an empty wall");
    ROOT_TOKEN.call_once(|| token);
}

/// The root domain's authority token, if the wall has been initialized.
#[allow(dead_code)] // presented/delegated by the bootstrap authority seams (wired incrementally)
pub fn root_token() -> Option<CapToken> {
    ROOT_TOKEN.get().copied()
}

/// Mint a capability granting `authority` to `domain`. *Policy* (whether this
/// should happen) is the caller's; the wall only provides the mechanism. Returns
/// `None` if the wall is uninitialized or at capacity.
#[allow(dead_code)] // consumed by delegation seams (wired incrementally)
pub fn issue(domain: DomainId, authority: Authority) -> Option<CapToken> {
    WALL.get()?.lock().issue(domain, authority).ok()
}

/// Authorize an operation: does `token` currently convey `needed` for `domain`?
/// Forged, revoked, wrong-domain, or insufficient tokens return `false`. An
/// uninitialized wall denies by default.
#[allow(dead_code)] // consumed by cross-domain authorization seams (wired incrementally)
#[must_use]
pub fn authorize(token: CapToken, domain: DomainId, needed: Authority) -> bool {
    match WALL.get() {
        Some(wall) => wall.lock().authorizes(token, domain, needed),
        None => false,
    }
}

/// Revoke a capability by destroying its record; a generation bump invalidates
/// every outstanding copy of the token. Returns `true` if a live capability was
/// revoked.
#[allow(dead_code)] // consumed by revocation seams (wired incrementally)
pub fn revoke(token: CapToken) -> bool {
    match WALL.get() {
        Some(wall) => wall.lock().revoke(token),
        None => false,
    }
}
