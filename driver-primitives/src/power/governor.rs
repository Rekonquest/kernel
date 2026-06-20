//! CPU frequency (P-state) and idle (C-state) selection with hysteresis.
//!
//! A small policy orchestrator: step the P-state up when load is high and down
//! when it is low, with a dead-band between the two thresholds so the frequency
//! does not oscillate. The C-state deepens as idle residency rises. Pure policy,
//! no mechanism.

/// A frequency/idle governor over `P` P-states (frequencies, ascending).
pub struct Governor<const P: usize> {
    freqs: [u32; P],
    current: usize,
    up_threshold: u8,
    down_threshold: u8,
}

impl<const P: usize> Governor<P> {
    /// Create a governor. `freqs` must be ascending; `up`/`down` are load
    /// percentages with `down < up` providing hysteresis. Starts at the lowest
    /// P-state.
    pub fn new(freqs: [u32; P], up: u8, down: u8) -> Self {
        Self {
            freqs,
            current: 0,
            up_threshold: up,
            down_threshold: down,
        }
    }

    /// The current P-state index.
    pub fn index(&self) -> usize {
        self.current
    }

    /// The current frequency.
    pub fn frequency(&self) -> u32 {
        self.freqs[self.current.min(P.saturating_sub(1))]
    }

    /// Step toward the right P-state for `load` (0..=100) and return the new
    /// frequency. One step per call; the dead-band between thresholds keeps it
    /// from oscillating.
    pub fn select(&mut self, load: u8) -> u32 {
        if load >= self.up_threshold && self.current + 1 < P {
            self.current += 1;
        } else if load <= self.down_threshold && self.current > 0 {
            self.current -= 1;
        }
        self.frequency()
    }

    /// Choose an idle C-state (0 = shallow … `max_depth - 1` = deepest) from
    /// idle residency `idle` (0..=100). Deeper as the CPU idles more.
    pub fn c_state(&self, idle: u8, max_depth: usize) -> usize {
        if max_depth == 0 {
            return 0;
        }
        let depth = (idle as usize * max_depth) / 101;
        depth.min(max_depth - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gov() -> Governor<4> {
        // 800 MHz .. 3.2 GHz, step up at 80% load, down at 20%.
        Governor::new([800, 1600, 2400, 3200], 80, 20)
    }

    #[test]
    fn ramps_up_under_load_and_saturates() {
        let mut g = gov();
        assert_eq!(g.frequency(), 800);
        assert_eq!(g.select(90), 1600);
        assert_eq!(g.select(90), 2400);
        assert_eq!(g.select(90), 3200);
        assert_eq!(g.select(90), 3200); // capped at the top P-state
        assert_eq!(g.index(), 3);
    }

    #[test]
    fn ramps_down_when_idle_and_floors() {
        let mut g = gov();
        for _ in 0..3 {
            g.select(100); // climb to the top
        }
        assert_eq!(g.index(), 3);
        assert_eq!(g.select(5), 2400);
        assert_eq!(g.select(5), 1600);
        assert_eq!(g.select(5), 800);
        assert_eq!(g.select(5), 800); // floored at the bottom
    }

    #[test]
    fn mid_range_load_holds_the_pstate() {
        let mut g = gov();
        g.select(90); // -> index 1
        assert_eq!(g.index(), 1);
        // 50% is inside the dead-band (20..80): no change, no oscillation.
        assert_eq!(g.select(50), 1600);
        assert_eq!(g.select(50), 1600);
        assert_eq!(g.index(), 1);
    }

    #[test]
    fn deeper_c_state_with_more_idle() {
        let g = gov();
        assert_eq!(g.c_state(0, 4), 0);
        assert_eq!(g.c_state(100, 4), 3);
        assert_eq!(g.c_state(50, 4), 1);
        assert_eq!(g.c_state(50, 0), 0); // no idle states
    }
}
