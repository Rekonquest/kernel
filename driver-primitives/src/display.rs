//! Multi-screen display orchestrator — KMS-lite over the kit.
//!
//! A display controller is, structurally, two kit primitives wearing a graphics
//! costume:
//!
//! - **Atomic multi-output modeset** is the [`Transaction`] primitive. Configuring
//!   several monitors at once must be all-or-nothing — every screen takes the new
//!   configuration or none does, so a bad mode on one output never leaves the
//!   others half-reconfigured. That is exactly DRM's atomic `TEST_ONLY`-then-commit,
//!   and it *is* `Transaction::validate` + `Transaction::commit`.
//! - **Page-flip / vblank** is the [`Fence`] primitive. A flip is submitted, and
//!   its completion is the next vblank — a monotonic per-output timeline with
//!   [`SeqCounter`] dispensing flip tickets and [`Fence`] marking completion.
//!
//! [`Transaction`]: crate::txn::Transaction
//! [`Fence`]: crate::fence::Fence
//! [`SeqCounter`]: crate::fence::SeqCounter

use crate::fence::{Fence, SeqCounter};
use crate::txn::Transaction;

/// A display mode: resolution and refresh rate (milliHz, e.g. 60000 = 60 Hz).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
}

impl Mode {
    pub const fn new(width: u32, height: u32, refresh_mhz: u32) -> Self {
        Self {
            width,
            height,
            refresh_mhz,
        }
    }

    /// Whether this mode fits within `limit` on every axis.
    pub const fn fits_within(&self, limit: &Mode) -> bool {
        self.width <= limit.width && self.height <= limit.height && self.refresh_mhz <= limit.refresh_mhz
    }
}

/// An opaque framebuffer id — the scanout buffer an output displays.
pub type FbId = u32;

/// A staged change to one output, accumulated into an atomic commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Drive `output` at `mode`.
    SetMode { output: usize, mode: Mode },
    /// Scan out framebuffer `fb` on `output`.
    SetFb { output: usize, fb: FbId },
    /// Turn `output` off.
    Disable { output: usize },
}

/// One output: a connector (physical port) paired with its scanout engine.
#[derive(Clone, Copy, Debug)]
pub struct Output {
    connected: bool,
    max_mode: Mode,
    mode: Option<Mode>,
    fb: Option<FbId>,
    flip_seq: SeqCounter,
    vblank: Fence,
    pending_flip: Option<u64>,
}

impl Output {
    const DEFAULT: Output = Output {
        connected: false,
        max_mode: Mode::new(0, 0, 0),
        mode: None,
        fb: None,
        flip_seq: SeqCounter::new(),
        vblank: Fence::new(),
        pending_flip: None,
    };

    /// Whether a display is plugged into this output.
    pub const fn is_connected(&self) -> bool {
        self.connected
    }
    /// The largest mode this output supports (its EDID bound).
    pub const fn max_mode(&self) -> Mode {
        self.max_mode
    }
    /// The mode currently driven, if enabled.
    pub const fn mode(&self) -> Option<Mode> {
        self.mode
    }
    /// The framebuffer currently scanned out, if any.
    pub const fn fb(&self) -> Option<FbId> {
        self.fb
    }
}

/// A display controller with `N` outputs.
pub struct Display<const N: usize> {
    outputs: [Output; N],
}

impl<const N: usize> Default for Display<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Display<N> {
    /// Create a controller with all outputs disconnected.
    pub fn new() -> Self {
        Self {
            outputs: [Output::DEFAULT; N],
        }
    }

    /// Number of outputs.
    pub const fn output_count(&self) -> usize {
        N
    }

    /// Borrow an output.
    pub fn output(&self, index: usize) -> Option<&Output> {
        self.outputs.get(index)
    }

    /// Mark a display plugged into `index`, advertising `max_mode`.
    pub fn connect(&mut self, index: usize, max_mode: Mode) -> bool {
        match self.outputs.get_mut(index) {
            Some(o) => {
                o.connected = true;
                o.max_mode = max_mode;
                true
            }
            None => false,
        }
    }

    /// Mark `index` unplugged and turn it off.
    pub fn disconnect(&mut self, index: usize) {
        if let Some(o) = self.outputs.get_mut(index) {
            o.connected = false;
            o.mode = None;
            o.fb = None;
            o.pending_flip = None;
        }
    }

    /// How many outputs currently have a display connected.
    pub fn connected_count(&self) -> usize {
        self.outputs.iter().filter(|o| o.connected).count()
    }

    /// Validate a staged atomic modeset without applying anything (TEST_ONLY).
    pub fn check<const M: usize>(&self, txn: &Transaction<Change, M>) -> bool {
        txn.validate(|change| self.change_is_valid(change))
    }

    /// Atomically apply a staged modeset across every affected output: validate
    /// the whole batch, then commit it as a unit. On validation failure nothing
    /// changes — no screen is left half-configured.
    pub fn commit<const M: usize>(&mut self, txn: Transaction<Change, M>) -> Result<(), Transaction<Change, M>> {
        if !self.check(&txn) {
            return Err(txn);
        }
        txn.commit(|change| self.apply(change));
        Ok(())
    }

    /// Schedule a page flip on `output` to framebuffer `fb`. Returns the flip
    /// sequence whose completion is the next [`vblank`](Self::vblank), or `None`
    /// if the output is not enabled.
    pub fn flip(&mut self, output: usize, fb: FbId) -> Option<u64> {
        let o = self.outputs.get_mut(output)?;
        if o.mode.is_none() {
            return None;
        }
        let seq = o.flip_seq.next_point();
        o.pending_flip = Some(seq);
        o.fb = Some(fb);
        Some(seq)
    }

    /// Signal a vblank on `output` (the IRQ handler's job), completing any
    /// pending page flip.
    pub fn vblank(&mut self, output: usize) {
        if let Some(o) = self.outputs.get_mut(output) {
            if let Some(seq) = o.pending_flip.take() {
                o.vblank.signal(seq);
            }
        }
    }

    /// Whether the flip identified by `seq` on `output` has completed.
    pub fn flip_completed(&self, output: usize, seq: u64) -> bool {
        self.outputs
            .get(output)
            .is_some_and(|o| o.vblank.is_passed(seq))
    }

    fn change_is_valid(&self, change: &Change) -> bool {
        match *change {
            Change::SetMode { output, mode } => self
                .outputs
                .get(output)
                .is_some_and(|o| o.connected && mode.fits_within(&o.max_mode)),
            Change::SetFb { output, .. } => {
                self.outputs.get(output).is_some_and(|o| o.connected)
            }
            Change::Disable { output } => self.outputs.get(output).is_some(),
        }
    }

    fn apply(&mut self, change: Change) {
        match change {
            Change::SetMode { output, mode } => {
                if let Some(o) = self.outputs.get_mut(output) {
                    o.mode = Some(mode);
                }
            }
            Change::SetFb { output, fb } => {
                if let Some(o) = self.outputs.get_mut(output) {
                    o.fb = Some(fb);
                }
            }
            Change::Disable { output } => {
                if let Some(o) = self.outputs.get_mut(output) {
                    o.mode = None;
                    o.fb = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_1080p() -> Mode {
        Mode::new(1920, 1080, 60000)
    }
    fn mode_4k() -> Mode {
        Mode::new(3840, 2160, 60000)
    }

    #[test]
    fn atomic_modeset_configures_every_screen_at_once() {
        let mut display: Display<4> = Display::new();
        display.connect(0, mode_4k());
        display.connect(1, mode_4k());
        display.connect(2, mode_1080p());
        assert_eq!(display.connected_count(), 3);

        // One commit reconfigures three monitors.
        let mut txn: Transaction<Change, 8> = Transaction::new();
        txn.stage(Change::SetMode { output: 0, mode: mode_4k() }).unwrap();
        txn.stage(Change::SetFb { output: 0, fb: 10 }).unwrap();
        txn.stage(Change::SetMode { output: 1, mode: mode_1080p() }).unwrap();
        txn.stage(Change::SetFb { output: 1, fb: 11 }).unwrap();
        txn.stage(Change::SetMode { output: 2, mode: mode_1080p() }).unwrap();
        txn.stage(Change::SetFb { output: 2, fb: 12 }).unwrap();

        assert!(display.commit(txn).is_ok());
        assert_eq!(display.output(0).unwrap().mode(), Some(mode_4k()));
        assert_eq!(display.output(1).unwrap().mode(), Some(mode_1080p()));
        assert_eq!(display.output(2).unwrap().fb(), Some(12));
    }

    #[test]
    fn a_bad_change_aborts_the_whole_commit() {
        let mut display: Display<4> = Display::new();
        display.connect(0, mode_4k());
        // output 3 is disconnected.

        // Establish a baseline on output 0.
        let mut base: Transaction<Change, 2> = Transaction::new();
        base.stage(Change::SetMode { output: 0, mode: mode_1080p() }).unwrap();
        display.commit(base).unwrap();
        assert_eq!(display.output(0).unwrap().mode(), Some(mode_1080p()));

        // Now attempt an atomic commit that changes output 0 *and* a disconnected
        // output. Validation must fail and leave output 0 untouched.
        let mut bad: Transaction<Change, 4> = Transaction::new();
        bad.stage(Change::SetMode { output: 0, mode: mode_4k() }).unwrap();
        bad.stage(Change::SetMode { output: 3, mode: mode_1080p() }).unwrap(); // disconnected
        assert!(display.commit(bad).is_err());
        // Output 0 still has the old mode — nothing was half-applied.
        assert_eq!(display.output(0).unwrap().mode(), Some(mode_1080p()));
    }

    #[test]
    fn mode_exceeding_the_panel_is_rejected() {
        let mut display: Display<2> = Display::new();
        display.connect(0, mode_1080p()); // panel maxes at 1080p

        let mut txn: Transaction<Change, 2> = Transaction::new();
        txn.stage(Change::SetMode { output: 0, mode: mode_4k() }).unwrap(); // too big
        assert!(!display.check(&txn));
        assert!(display.commit(txn).is_err());
        assert_eq!(display.output(0).unwrap().mode(), None);
    }

    #[test]
    fn page_flip_completes_on_vblank() {
        let mut display: Display<2> = Display::new();
        display.connect(0, mode_4k());
        let mut txn: Transaction<Change, 2> = Transaction::new();
        txn.stage(Change::SetMode { output: 0, mode: mode_4k() }).unwrap();
        display.commit(txn).unwrap();

        let seq = display.flip(0, 42).unwrap();
        assert!(!display.flip_completed(0, seq)); // not until vblank
        display.vblank(0);
        assert!(display.flip_completed(0, seq));
        assert_eq!(display.output(0).unwrap().fb(), Some(42));
    }

    #[test]
    fn flip_on_disabled_output_is_refused() {
        let mut display: Display<2> = Display::new();
        display.connect(0, mode_4k()); // connected but no mode set
        assert_eq!(display.flip(0, 1), None);
    }
}
