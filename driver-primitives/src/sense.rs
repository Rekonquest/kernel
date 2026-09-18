//! Sense field — a behavior sensor primitive (dumb mechanism, zero policy).
//!
//! This is the kit's *behavior* counterpart to [`AuthorityWall`]
//! (which gates **construction**): the sense field watches what
//! running domains **do**. Like every primitive here it is dumb on
//! purpose — it records vibrations on a small substrate field, and
//! the field's material state (scars) is the only memory. The
//! orchestrator (the kernel) decides which seams feed it and which
//! gates consult it; the field holds no policy about that.
//!
//! ## The law (same substrate lineage as the atom program's sensors)
//!
//! A **vibration** is `(pid, class, target)` — behavior, never
//! payload. It lands on one of `SITES` field sites
//! (`mix(pid, target, class) % SITES`): activity grows the site's
//! permeability; erosion relaxes every site unconditionally
//! (rate-decoupled: one erosion pass per `EROSION_INTERVAL` events,
//! so a site's equilibrium reflects its **absolute** activity, not
//! its share of total traffic). A site readable above
//! [`READABLE`] has been in *sustained* use.
//!
//! ## Learning, freeze, foreign budget, quarantine
//!
//! The field starts **learning**: everything scarring is normal by
//! definition. After [`LEARNING_EVENTS`] vibrations (or an explicit
//! [`freeze`](SenseField::freeze)) the **normal map** is snapshotted:
//! every readable site, dilated one Moore ring — *who talks to whom,
//! as material state*. Post-freeze, a vibration landing on an
//! unlearned site spends **foreign budget** for its pid (a budget,
//! not a threshold: one stray event never condemns; sustained
//! misuse always does). Budget decays per event
//! ([`BUDGET_DECAY`]) — condemnation is thermodynamic, not
//! permanent: a wrongly-condemned domain recovers with clean
//! behavior, while a real offender re-charges the budget the moment
//! it resumes.
//!
//! [`quarantined`](SenseField::quarantined) is the one-bit verdict.
//! [`take_intrusion`](SenseField::take_intrusion) drains condemned
//! pids (each reported exactly once) for the orchestrator's
//! listeners — e.g. coupling behavior-condemnation into capability
//! revocation.
//!
//! ## Doctrine (shared with `authority.rs`)
//!
//! Dumb mechanism only; identity is *named by the kernel* (the pid
//! the orchestrator passes in), never asserted; no policy about who
//! deserves quarantine lives here.
//!
//! [`AuthorityWall`]: crate::authority::AuthorityWall

/// Field geometry: 128 conversation sites.
pub const SITES: usize = 128;
/// Events recorded before auto-freeze.
pub const LEARNING_EVENTS: u64 = 4096;
/// Permeability law: virgin floor and sustained-use readability.
pub const FLOOR: f32 = 0.05;
pub const READABLE: f32 = 0.14;
const FORMATION: f32 = 0.03;
const EROSION: f32 = 0.0018;
/// Rate-decoupled erosion: one pass per 16 events (a site's
/// equilibrium tracks absolute activity, not traffic share).
const EROSION_INTERVAL: u64 = 16;
/// Budget above which a pid is quarantined: sustained misuse, never
/// a single stray event.
pub const QUARANTINE_BUDGET: f32 = 2.0;
/// Budget decay per decay interval (thermodynamic release,
/// rate-decoupled like erosion: per-event decay at this size nearly
/// cancels the 0.02 charge — foreign growth stalls at ~0.007/event
/// and condemnation never arrives; probe-taught).
const BUDGET_DECAY: f32 = 0.013;
const BUDGET_DECAY_INTERVAL: u64 = 8;

/// Tracked-pid capacity for foreign budgets (loud pids carry the
/// verdict; a full table drops newcomers — honest degradation).
const TRACKED: usize = 16;

#[inline]
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Deterministic site for a `(pid, target, class)` conversation.
/// The WHOLE tuple is mixed before the modulus — mixing only one
/// term left `(target << 17) % 128 == 0` for every target (bits
/// above the seventh vanish), making the site target-blind (a
/// probe-taught bug: pid 5 landed every partner on one site).
pub fn site_for(pid: u64, target: u64, class: u64) -> usize {
    (mix(pid ^ (target << 17) ^ class.wrapping_mul(0x9E37_79B9_7F4A_7C15)) % SITES as u64)
        as usize
}

const fn mix_const(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The sense field. Clone for snapshots; all state inline, no alloc.
#[derive(Clone)]
pub struct SenseField {
    permeability: [f32; SITES],
    foreign: [(u64, f32); TRACKED],
    trained: bool,
    normal_map: [bool; SITES],
    events: u64,
    /// Condemned pids awaiting the orchestrator (u64::MAX = empty).
    intrusions: [u64; 4],
}

impl Default for SenseField {
    fn default() -> Self {
        Self::new()
    }
}

impl SenseField {
    pub fn new() -> Self {
        Self {
            permeability: [FLOOR; SITES],
            foreign: [(0, 0.0); TRACKED],
            trained: false,
            normal_map: [false; SITES],
            events: 0,
            intrusions: [u64::MAX; 4],
        }
    }

    /// Record one vibration. Cheap by law: a mix, two adds, the
    /// material tick. Learning phase scars silently; post-freeze,
    /// foreign landings spend budget and (on quarantine) queue the
    /// pid exactly once for the orchestrator.
    pub fn record(&mut self, pid: u64, class: u64, target: u64, weight: f32) {
        let site = site_for(pid, target, class);
        let energy = weight * 0.02;
        let growth = FORMATION * energy.min(1.0) * (1.0 - self.permeability[site]);
        self.permeability[site] = (self.permeability[site] + growth).clamp(FLOOR, 1.0);
        self.events += 1;
        if self.events % EROSION_INTERVAL == 0 {
            for p in self.permeability.iter_mut() {
                *p -= EROSION * (*p - FLOOR).max(0.0);
                if *p < FLOOR {
                    *p = FLOOR;
                }
            }
        }

        // Budget decay is UNCONDITIONAL (same law as erosion) and
        // rate-decoupled: clean traffic releases a wrongly-condemned
        // domain, while a rogue's own events cannot out-decay its
        // charge.
        if self.events % BUDGET_DECAY_INTERVAL == 0 {
            for (tracked, budget) in self.foreign.iter_mut() {
                let _ = tracked;
                if *budget > 0.0 {
                    *budget = (*budget - BUDGET_DECAY).max(0.0);
                }
            }
        }

        if !self.trained {
            if self.events >= LEARNING_EVENTS {
                self.freeze();
            }
            return;
        }
        if !self.normal_map[site] {
            let mut charged = false;
            for (tracked, budget) in self.foreign.iter_mut() {
                if *tracked == pid {
                    *budget += energy;
                    charged = true;
                    break;
                }
            }
            if !charged {
                if let Some(entry) = self.foreign.iter_mut().find(|(t, _)| *t == 0) {
                    *entry = (pid, energy);
                }
            }
            if self.quarantined(pid) {
                self.queue_intrusion(pid);
            }
        }
    }

    fn queue_intrusion(&mut self, pid: u64) {
        if self.intrusions.contains(&pid) {
            return;
        }
        if let Some(slot) = self.intrusions.iter_mut().find(|s| **s == u64::MAX) {
            *slot = pid;
        }
    }

    /// Snapshot the normal map: readable sites (sustained use), each
    /// dilated one Moore ring. Refuses to freeze on an empty field.
    pub fn freeze(&mut self) {
        let mut raised = [false; SITES];
        let mut any = false;
        for (site, &p) in self.permeability.iter().enumerate() {
            if p > READABLE {
                raised[site] = true;
                any = true;
            }
        }
        if !any {
            return;
        }
        let mut map = raised;
        const W: usize = 16;
        const H: usize = SITES / W;
        for site in 0..SITES {
            if !raised[site] {
                continue;
            }
            let (x, y) = (site % W, site / W);
            for dy in [-1i32, 0, 1] {
                for dx in [-1i32, 0, 1] {
                    let nx = x as i32 + dx;
                    let ny = y as i32 + dy;
                    if (0..W as i32).contains(&nx) && (0..H as i32).contains(&ny) {
                        map[ny as usize * W + nx as usize] = true;
                    }
                }
            }
        }
        self.normal_map = map;
        self.trained = true;
    }

    /// Foreign budget a pid has accumulated (0 when untracked).
    pub fn foreign_budget(&self, pid: u64) -> f32 {
        self.foreign
            .iter()
            .find(|(tracked, _)| *tracked == pid)
            .map(|(_, b)| *b)
            .unwrap_or(0.0)
    }

    /// The verdict: sustained foreign behavior. A single stray event
    /// never trips this (budget, not threshold).
    pub fn quarantined(&self, pid: u64) -> bool {
        self.trained
            && self
                .foreign
                .iter()
                .any(|(tracked, budget)| *tracked == pid && *budget > QUARANTINE_BUDGET)
    }

    /// Drain one condemned pid (each reported exactly once), for the
    /// orchestrator's listeners (e.g. capability revocation).
    pub fn take_intrusion(&mut self) -> Option<u64> {
        if let Some(slot) = self.intrusions.iter_mut().find(|s| **s != u64::MAX) {
            let pid = *slot;
            *slot = u64::MAX;
            Some(pid)
        } else {
            None
        }
    }

    /// Forget a dead pid's tracking (pid-reuse starts clean).
    pub fn forget(&mut self, pid: u64) {
        if let Some((tracked, budget)) = self.foreign.iter_mut().find(|(t, _)| *t == pid) {
            *tracked = 0;
            *budget = 0.0;
        }
    }

    /// Status: (trained, events, raised sites, readable sites).
    pub fn status(&self) -> (bool, u64, usize, usize) {
        (
            self.trained,
            self.events,
            self.permeability.iter().filter(|&&p| p > FLOOR + 0.005).count(),
            self.permeability.iter().filter(|&&p| p > READABLE).count(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clean conversation pattern: pids 1-3 talking to partners 1-2
    /// through classes 5/15 — the learning vocabulary.
    fn clean_boot(events: usize) {
        let mut field = SenseField::new();
        for i in 0..events {
            let pid = (i % 3) as u64 + 1;
            let target = ((i / 3) % 2) as u64 + 1;
            let class = if i % 2 == 0 { 15 } else { 5 };
            field.record(pid, class, target, 1.0);
        }
        FIELD.with(|f| *f.borrow_mut() = field);
    }

    use std::cell::RefCell;
    thread_local! {
        static FIELD: RefCell<SenseField> = RefCell::new(SenseField::new());
    }

    fn rec(pid: u64, class: u64, target: u64) {
        FIELD.with(|f| f.borrow_mut().record(pid, class, target, 1.0));
    }

    #[test]
    fn gate_r1_learning_freeze_clean_admits() {
        clean_boot(6000);
        FIELD.with(|f| {
            let mut f = f.borrow_mut();
            f.freeze();
            let (trained, events, raised, _) = f.status();
            assert!(trained && events == 6000 && raised > 0);
        });
        // Post-freeze, the same clean vocabulary charges no one.
        for i in 0..600 {
            let pid = (i % 3) as u64 + 1;
            let target = ((i / 3) % 2) as u64 + 1;
            let class = if i % 2 == 0 { 15 } else { 5 };
            rec(pid, class, target);
        }
        for pid in 1..=3 {
            FIELD.with(|f| assert_eq!(f.borrow().foreign_budget(pid), 0.0));
            FIELD.with(|f| assert!(!f.borrow().quarantined(pid)));
        }
    }

    #[test]
    fn gate_r2_rogue_conversation_condemned_and_reported() {
        clean_boot(6000);
        FIELD.with(|f| f.borrow_mut().freeze());
        // A rogue pid scanning partners the boot never used (spread:
        // a single partner can luck into the dilated map).
        for i in 0..600u64 {
            rec(9, 15, 90 + i % 32);
        }
        FIELD.with(|f| {
            let f = f.borrow();
            assert!(f.quarantined(9), "budget {}", f.foreign_budget(9));
        });
        // The intrusion is reported exactly once.
        let first = FIELD.with(|f| f.borrow_mut().take_intrusion());
        assert_eq!(first, Some(9));
        let second = FIELD.with(|f| f.borrow_mut().take_intrusion());
        assert_eq!(second, None);
    }

    #[test]
    fn gate_r3_budget_not_threshold() {
        clean_boot(6000);
        FIELD.with(|f| f.borrow_mut().freeze());
        rec(4, 15, 200); // one stray event
        FIELD.with(|f| assert!(!f.borrow().quarantined(4)));
    }

    #[test]
    fn gate_r4_thermodynamic_release_and_recharge() {
        clean_boot(6000);
        FIELD.with(|f| f.borrow_mut().freeze());
        // Spread targets (the recurring lesson): a single partner can
        // land inside the dilated normal map and never charge.
        for i in 0..500u64 {
            rec(5, 15, 320 + i % 32);
        }
        FIELD.with(|f| assert!(f.borrow().quarantined(5)));
        // Quiet budget decay: the honest thermodynamic timescale —
        // 0.013 per 8 events sheds the rogue's ~6 budget in ~4,000
        // clean events. Release is slower than condemnation by
        // design (trust erodes slower than it burns).
        for i in 0..5000 {
            let pid = (i % 3) as u64 + 1;
            let target = ((i / 3) % 2) as u64 + 1;
            let class = if i % 2 == 0 { 15 } else { 5 };
            rec(pid, class, target);
        }
        FIELD.with(|f| assert!(!f.borrow().quarantined(5)));
        // Resume misuse (spread): re-condemned quickly.
        for i in 0..300u64 {
            rec(5, 15, 360 + i % 32);
        }
        FIELD.with(|f| assert!(f.borrow().quarantined(5)));
    }

    #[test]
    fn gate_r5_forget_cleans_pid_reuse() {
        clean_boot(6000);
        FIELD.with(|f| f.borrow_mut().freeze());
        for i in 0..500u64 {
            rec(6, 15, 400 + i % 32);
        }
        FIELD.with(|f| assert!(f.borrow().quarantined(6)));
        FIELD.with(|f| f.borrow_mut().forget(6));
        FIELD.with(|f| assert!(!f.borrow().quarantined(6)));
    }
}
