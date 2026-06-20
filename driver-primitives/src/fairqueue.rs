//! Virtual-time fair arbitration — one scheduler for every contended resource.
//!
//! This is the cross-domain primitive. A CPU scheduler (CFS/EEVDF), a GPU
//! context arbiter, network flow fairness (WFQ/fq_codel), block-device weighted
//! queueing, an audio mixer's time-share, and TDMA radio-slot assignment are
//! the *same* mechanism wearing different clothes: among clients contending for
//! one resource, hand the next turn to whoever has received the least service
//! relative to its weight. Each domain reinvents it; here it is once, dumb and
//! domain-agnostic.
//!
//! A **client** is any contender — thread, GPU context, NIC flow, NVMe
//! namespace, audio stream, radio link. A **cost** is any scalar unit of the
//! resource — nanoseconds, bytes, IOPS, slots. [`FairQueue`] decides *who* goes
//! next and nothing else: it moves no data and does not know what the resource
//! is. It also cannot *create* capacity — that is the real wall
//! (work-conservation / the channel's throughput). It only allocates the
//! capacity that exists, in proportion to weight.
//!
//! The math is the kernel's EEVDF core (`min vruntime first`, weighted by
//! priority) lifted out of the scheduler so it is reusable and host-testable.

/// Fixed-point scale for weighted virtual time. Keeps proportional shares
/// precise without floating point.
const SCALE: u128 = 1 << 20;

#[derive(Clone, Copy, Debug)]
struct Client {
    active: bool,
    backlogged: bool,
    weight: u64,
    /// Weighted virtual time: accumulated service divided by weight.
    vtime: u64,
}

impl Client {
    const EMPTY: Client = Client {
        active: false,
        backlogged: false,
        weight: 1,
        vtime: 0,
    };
}

/// A weighted virtual-time arbiter over `N` client slots, addressed by index.
#[derive(Debug)]
pub struct FairQueue<const N: usize> {
    clients: [Client; N],
    /// Floor virtual time. Newly-activated clients start here so they neither
    /// receive a windfall of catch-up service nor are starved on entry. This is
    /// exactly CFS's `min_vruntime`.
    min_vtime: u64,
}

impl<const N: usize> Default for FairQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> FairQueue<N> {
    /// Create an arbiter with all `N` slots idle.
    pub const fn new() -> Self {
        Self {
            clients: [Client::EMPTY; N],
            min_vtime: 0,
        }
    }

    /// Admit client `id` with the given `weight` (clamped to `>= 1`). Its
    /// virtual time starts at the current floor, so it competes fairly from now
    /// rather than retroactively.
    pub fn activate(&mut self, id: usize, weight: u64) {
        if let Some(c) = self.clients.get_mut(id) {
            c.active = true;
            c.backlogged = false;
            c.weight = weight.max(1);
            c.vtime = self.min_vtime;
        }
    }

    /// Remove client `id` from contention.
    pub fn deactivate(&mut self, id: usize) {
        if let Some(c) = self.clients.get_mut(id) {
            *c = Client::EMPTY;
        }
        self.min_vtime = self.min_active_vtime().unwrap_or(self.min_vtime);
    }

    /// Mark whether client `id` currently has work waiting. Only a backlogged,
    /// active client is eligible to be [`pick`]ed.
    ///
    /// [`pick`]: FairQueue::pick
    pub fn set_backlogged(&mut self, id: usize, backlogged: bool) {
        if let Some(c) = self.clients.get_mut(id) {
            if c.active {
                c.backlogged = backlogged;
            }
        }
    }

    /// Choose the next client to serve: the backlogged, active client with the
    /// least weighted virtual time. Ties break toward the lowest id. Returns
    /// `None` if nothing is eligible (the resource may idle — work-conserving).
    pub fn pick(&self) -> Option<usize> {
        let mut best: Option<(usize, u64)> = None;
        for (id, c) in self.clients.iter().enumerate() {
            if c.active && c.backlogged {
                match best {
                    Some((_, best_vtime)) if c.vtime >= best_vtime => {}
                    _ => best = Some((id, c.vtime)),
                }
            }
        }
        best.map(|(id, _)| id)
    }

    /// Account `cost` units of service to client `id`, advancing its virtual
    /// time by `cost / weight`. Call after serving the client. Slower-advancing
    /// (higher-weight) clients are picked more often — that is the fairness.
    pub fn charge(&mut self, id: usize, cost: u64) {
        if let Some(c) = self.clients.get_mut(id) {
            if c.active {
                let delta = (cost as u128 * SCALE / c.weight as u128) as u64;
                c.vtime = c.vtime.saturating_add(delta);
            }
        }
        // The floor tracks the least-serviced active client.
        self.min_vtime = self.min_active_vtime().unwrap_or(self.min_vtime);
    }

    /// Weighted virtual time of client `id`, if active.
    pub fn vtime_of(&self, id: usize) -> Option<u64> {
        self.clients.get(id).filter(|c| c.active).map(|c| c.vtime)
    }

    /// Whether any client currently has work to serve.
    pub fn has_work(&self) -> bool {
        self.clients.iter().any(|c| c.active && c.backlogged)
    }

    fn min_active_vtime(&self) -> Option<u64> {
        self.clients
            .iter()
            .filter(|c| c.active)
            .map(|c| c.vtime)
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_only_backlogged_active_clients() {
        let mut fq: FairQueue<3> = FairQueue::new();
        assert_eq!(fq.pick(), None);
        fq.activate(0, 1);
        assert_eq!(fq.pick(), None); // active but no work
        fq.set_backlogged(0, true);
        assert_eq!(fq.pick(), Some(0));
        fq.set_backlogged(0, false);
        assert_eq!(fq.pick(), None);
        assert!(!fq.has_work());
    }

    #[test]
    fn service_is_proportional_to_weight() {
        // The fairness property, the same one CFS/WFQ guarantee: with equal-cost
        // turns, a weight-3 client gets ~3x the service of a weight-1 client.
        let mut fq: FairQueue<2> = FairQueue::new();
        fq.activate(0, 1);
        fq.activate(1, 3);
        fq.set_backlogged(0, true);
        fq.set_backlogged(1, true);

        let mut count = [0u32; 2];
        for _ in 0..4000 {
            let id = fq.pick().unwrap();
            count[id] += 1;
            fq.charge(id, 1); // each turn costs one unit
        }

        assert_eq!(count[0] + count[1], 4000);
        // Expect ~1000 / ~3000; allow a small margin for integer rounding.
        assert!((990..=1010).contains(&count[0]), "count = {count:?}");
        assert!((2990..=3010).contains(&count[1]), "count = {count:?}");
    }

    #[test]
    fn late_joiner_is_not_starved_and_does_not_burst() {
        // A client that joins after another has run for a long time starts at
        // the current floor — it neither gets a huge catch-up nor is locked out.
        let mut fq: FairQueue<2> = FairQueue::new();
        fq.activate(0, 1);
        fq.set_backlogged(0, true);
        for _ in 0..100 {
            let id = fq.pick().unwrap();
            fq.charge(id, 1);
        }
        // Client 1 joins now. It starts at the floor, which (client 0 being the
        // only active client) is client 0's current vtime — no windfall.
        fq.activate(1, 1);
        fq.set_backlogged(1, true);
        assert_eq!(fq.vtime_of(1), fq.vtime_of(0));
        // From here, equal weights => roughly equal service going forward.
        let mut count = [0u32; 2];
        for _ in 0..2000 {
            let id = fq.pick().unwrap();
            count[id] += 1;
            fq.charge(id, 1);
        }
        let diff = (count[0] as i64 - count[1] as i64).abs();
        assert!(diff < 60, "counts diverged: {count:?}");
    }

    #[test]
    fn cost_weighting_tracks_actual_work() {
        // Costs need not be uniform: a client doing big units advances faster.
        let mut fq: FairQueue<2> = FairQueue::new();
        fq.activate(0, 1);
        fq.activate(1, 1);
        fq.set_backlogged(0, true);
        fq.set_backlogged(1, true);

        // Client 0 always does 4x the work per turn it is given.
        let mut work = [0u64; 2];
        for _ in 0..2000 {
            let id = fq.pick().unwrap();
            let cost = if id == 0 { 4 } else { 1 };
            work[id] += cost;
            fq.charge(id, cost);
        }
        // Equal weights => equal *work* (vtime), not equal turn counts.
        let diff = (work[0] as i64 - work[1] as i64).abs();
        assert!(diff < 40, "work diverged: {work:?}");
    }
}
