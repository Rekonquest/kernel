//! Weighted power/thermal budget allocation — `FairQueue`'s allocation cousin.
//!
//! A fixed budget (watts, or a thermal-headroom number) is shared across
//! components by weight, each capped at what it actually requests; headroom left
//! by a component that wants less than its share is redistributed to components
//! that still want more (weighted max-min fair). A component granted less than
//! it requested is *throttled*. Same weighted-share idea as the scheduler's
//! `FairQueue`, but over a divisible resource instead of time.

/// Allocates a power budget across `N` components addressed by index.
pub struct PowerBudget<const N: usize> {
    total: u32,
    active: [bool; N],
    weight: [u32; N],
    request: [u32; N],
    grant: [u32; N],
}

impl<const N: usize> Default for PowerBudget<N> {
    fn default() -> Self {
        Self::new(0)
    }
}

impl<const N: usize> PowerBudget<N> {
    /// Create an allocator with the given `total` budget and no components.
    pub const fn new(total: u32) -> Self {
        Self {
            total,
            active: [false; N],
            weight: [1; N],
            request: [0; N],
            grant: [0; N],
        }
    }

    /// Change the total budget (recompute with [`allocate`](Self::allocate)).
    pub fn set_total(&mut self, total: u32) {
        self.total = total;
    }

    /// Add or update component `id` with a `weight` (clamped `>= 1`) and a
    /// `request` (how much it wants).
    pub fn set_component(&mut self, id: usize, weight: u32, request: u32) {
        if id < N {
            self.active[id] = true;
            self.weight[id] = weight.max(1);
            self.request[id] = request;
        }
    }

    /// Drop component `id` from the budget.
    pub fn remove(&mut self, id: usize) {
        if id < N {
            self.active[id] = false;
            self.weight[id] = 1;
            self.request[id] = 0;
            self.grant[id] = 0;
        }
    }

    /// Recompute grants: weighted max-min fair allocation within `total`.
    pub fn allocate(&mut self) {
        for g in self.grant.iter_mut() {
            *g = 0;
        }
        let mut remaining = self.total;

        // Each pass either satisfies at least one component or stops, so N+1
        // passes suffice.
        for _ in 0..=N {
            if remaining == 0 {
                break;
            }
            let mut total_weight: u64 = 0;
            for i in 0..N {
                if self.active[i] && self.grant[i] < self.request[i] {
                    total_weight += self.weight[i] as u64;
                }
            }
            if total_weight == 0 {
                break;
            }

            let pass_budget = remaining;
            let mut distributed = 0u32;
            for i in 0..N {
                if self.active[i] && self.grant[i] < self.request[i] {
                    let share = (pass_budget as u64 * self.weight[i] as u64 / total_weight) as u32;
                    let want = self.request[i] - self.grant[i];
                    let add = share.min(want);
                    self.grant[i] += add;
                    distributed += add;
                }
            }

            if distributed == 0 {
                // Integer rounding left a remainder smaller than any share; hand
                // it out one unit at a time, then stop.
                for i in 0..N {
                    if remaining == 0 {
                        break;
                    }
                    if self.active[i] && self.grant[i] < self.request[i] {
                        self.grant[i] += 1;
                        remaining -= 1;
                    }
                }
                break;
            }
            remaining = self.total - self.granted_total();
        }
    }

    /// The budget granted to component `id`.
    pub fn grant(&self, id: usize) -> u32 {
        if id < N {
            self.grant[id]
        } else {
            0
        }
    }

    /// Whether component `id` got less than it requested.
    pub fn is_throttled(&self, id: usize) -> bool {
        id < N && self.active[id] && self.grant[id] < self.request[id]
    }

    /// Total budget currently handed out (never exceeds the total).
    pub fn granted_total(&self) -> u32 {
        self.grant.iter().sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_are_proportional_to_weight_when_all_want_more() {
        let mut budget: PowerBudget<2> = PowerBudget::new(100);
        budget.set_component(0, 1, 100);
        budget.set_component(1, 3, 100);
        budget.allocate();
        assert_eq!(budget.grant(0), 25);
        assert_eq!(budget.grant(1), 75);
        assert!(budget.is_throttled(0));
        assert!(budget.is_throttled(1));
        assert_eq!(budget.granted_total(), 100);
    }

    #[test]
    fn unused_headroom_is_redistributed() {
        let mut budget: PowerBudget<2> = PowerBudget::new(100);
        budget.set_component(0, 1, 10); // wants little
        budget.set_component(1, 1, 100); // wants lots
        budget.allocate();
        assert_eq!(budget.grant(0), 10); // satisfied
        assert!(!budget.is_throttled(0));
        assert_eq!(budget.grant(1), 90); // got the leftover
        assert!(budget.is_throttled(1));
        assert_eq!(budget.granted_total(), 100);
    }

    #[test]
    fn never_exceeds_total_and_under_demand_is_fully_met() {
        let mut budget: PowerBudget<3> = PowerBudget::new(100);
        budget.set_component(0, 1, 10);
        budget.set_component(1, 2, 20);
        budget.set_component(2, 3, 30);
        budget.allocate(); // total demand 60 < 100
        assert_eq!(budget.grant(0), 10);
        assert_eq!(budget.grant(1), 20);
        assert_eq!(budget.grant(2), 30);
        assert!(!budget.is_throttled(0));
        assert!(!budget.is_throttled(1));
        assert!(!budget.is_throttled(2));
        assert!(budget.granted_total() <= 100);
    }
}
