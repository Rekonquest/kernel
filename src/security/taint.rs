//! Taint layer — derived content stays data until a human promotes it.
//!
//! The zero-click wall 4 (E32's architecture, kernel edition). Anything
//! produced from untrusted input (network-received data, parsed
//! previews, extracted files) carries a TAINT mark: readable,
//! displayable, NEVER executable. The only path to execution is the
//! HUMAN PROMOTION event — a function no automatic path calls (wired
//! solely to the input-device scheme; the kernel's network, fs, and
//! scheme paths hold no reference to it).
//!
//! Taint is TRANSITIVE: a file description opened FROM tainted data
//! inherits the mark. The gate is at execution time (PROT_EXEC fmap
//! and the exec path), not at open time — opening and reading tainted
//! content is fine (previews are legitimate); EXECUTING it is not.
//!
//! Doctrine (same as the AuthorityWall): dumb mechanism, zero policy.
//! WHAT is untrusted is decided by the marking seams (the network
//! schemes); WHO may promote is decided by the caller identity (only
//! the input-device scheme). The registry itself just remembers.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use spin::{Mutex, Once};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaintState {
    /// Clean (trusted origin: the root filesystem, build artifacts).
    Clean,
    /// Derived from untrusted input: readable, displayable, NOT
    /// executable.
    Tainted,
    /// Explicitly promoted by the human input event.
    Promoted,
}

struct TaintRegistry {
    /// Keyed by (pid, fd) — file-description-granular taint.
    entries: BTreeMap<(u64, u64), TaintState>,
    /// The total promotions granted this boot (for audit).
    promotions: u64,
}

static REGISTRY: Once<Mutex<TaintRegistry>> = Once::new();

fn registry() -> &'static Mutex<TaintRegistry> {
    REGISTRY.call_once(|| {
        Mutex::new(TaintRegistry {
            entries: BTreeMap::new(),
            promotions: 0,
        })
    })
}

/// Initialize the taint layer (called from security::init).
pub fn init() {
    let _ = registry();
}

/// Mark a file description tainted (the network ingress seam).
/// Called when a network scheme (tcp/udp) delivers data to userspace.
pub fn mark_tainted(pid: u64, fd: u64) {
    let mut reg = registry().lock();
    reg.entries.insert((pid, fd), TaintState::Tainted);
}

/// Taint propagation: a new fd opened FROM a tainted source inherits
/// the mark. Called at the fs open seam when the opener holds a
/// tainted description.
pub fn propagate_taint(from_pid: u64, from_fd: u64, to_pid: u64, to_fd: u64) {
    let reg = registry().lock();
    if matches!(reg.entries.get(&(from_pid, from_fd)), Some(TaintState::Tainted)) {
        drop(reg);
        let mut reg = registry().lock();
        reg.entries.insert((to_pid, to_fd), TaintState::Tainted);
    }
}

/// THE HUMAN PROMOTION EVENT. The only path from Tainted to Promoted.
///
/// SECURITY INVARIANT: this function is called ONLY from the input-
/// device scheme handler (serio/input) — the path that begins with a
/// physical human input event. No network, fs, scheme, or timer path
/// holds a reference to it. The `caller_must_be_input_scheme` check
/// is a second belt: only the input scheme's fixed pid may promote.
pub fn promote(caller_pid: u64, input_scheme_pid: u64, target_pid: u64, target_fd: u64) -> bool {
    // The caller must BE the input scheme (structural: the kernel
    // passes the serio scheme's pid; no other context matches).
    if caller_pid != input_scheme_pid {
        return false;
    }
    let mut reg = registry().lock();
    if let Some(state) = reg.entries.get_mut(&(target_pid, target_fd)) {
        if *state == TaintState::Tainted {
            *state = TaintState::Promoted;
            reg.promotions += 1;
            return true;
        }
    }
    false
}

/// The execution gate: may this pid execute via this fd?
/// Clean and Promoted pass; Tainted is refused.
pub fn gate_exec(pid: u64, fd: u64) -> bool {
    let reg = registry().lock();
    match reg.entries.get(&(pid, fd)) {
        Some(TaintState::Tainted) => false,
        _ => true, // Clean, Promoted, or unknown (not tracked = clean)
    }
}

/// Taint state for audit: (state, total_tracked, total_promotions).
pub fn status(pid: u64, fd: u64) -> (TaintState, usize, u64) {
    let reg = registry().lock();
    (
        reg.entries.get(&(pid, fd)).copied().unwrap_or(TaintState::Clean),
        reg.entries.len(),
        reg.promotions,
    )
}

/// Forget a dying context's entries (pid reuse starts clean).
pub fn on_context_exit(pid: u64) {
    let mut reg = registry().lock();
    let keys: Vec<(u64, u64)> = reg
        .entries
        .keys()
        .filter(|(p, _)| *p == pid)
        .copied()
        .collect();
    for key in keys {
        reg.entries.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tainted_refuses_execution_until_promoted() {
        // The core gate: tainted → refused; promoted → passes.
        // (Direct registry manipulation for testing; the kernel goes
        // through the public API.)
        mark_tainted(7, 3);
        assert!(!gate_exec(7, 3), "tainted must refuse");
        // Promotion requires the input scheme's pid.
        assert!(!promote(999, 1, 7, 3), "non-input caller cannot promote");
        assert!(promote(1, 1, 7, 3), "input scheme promotes");
        assert!(gate_exec(7, 3), "promoted passes");
    }

    #[test]
    fn unknown_fd_is_clean() {
        // Untracked = clean (the default is trust; taint is the mark,
        // not the absence of one).
        assert!(gate_exec(99, 99));
        assert_eq!(status(99, 99).0, TaintState::Clean);
    }

    #[test]
    fn taint_propagates_through_derivation() {
        mark_tainted(5, 1);
        propagate_taint(5, 1, 5, 2);
        assert!(!gate_exec(5, 2), "derived fd inherits taint");
        // Clean sources don't taint derivatives.
        propagate_taint(5, 99, 5, 3);
        assert!(gate_exec(5, 3), "clean source stays clean");
    }

    #[test]
    fn context_exit_cleans_entries() {
        mark_tainted(11, 0);
        on_context_exit(11);
        assert!(gate_exec(11, 0), "pid reuse starts clean");
    }
}
