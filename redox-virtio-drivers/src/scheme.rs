//! Scheme front-ends — the data-path queues that connect Redox scheme clients
//! to the driver.
//!
//! A network driver exposes a `network:` scheme (read = receive a frame,
//! write = send one); a GPU driver exposes a framebuffer scheme (write =
//! request a flip). The types here hold the queues that bridge client I/O and
//! the driver's data path:
//!
//! - the driver's IRQ handler calls [`NetScheme::deliver`] with each frame the
//!   `RxPool` produced; a client read pops one;
//! - a client write enqueues a frame, and the driver drains
//!   [`NetScheme::take_tx`] and hands each to `VirtioNet::transmit`.
//!
//! This logic is OS-agnostic and host-tested. The actual scheme *server* — the
//! `SchemeMut`/packet loop that turns client syscalls into these calls — uses
//! the modern `redox-scheme` crate and runs in one event loop multiplexed with
//! the device IRQ via the `event:` scheme; see `BOOTING.md`.

use std::collections::VecDeque;

/// `network:` scheme front-end: the received and to-transmit frame queues that
/// bridge clients and the driver.
#[derive(Default)]
pub struct NetScheme {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}

impl NetScheme {
    /// Driver → scheme: a frame arrived (from the `RxPool`); make it available
    /// to readers.
    pub fn deliver(&mut self, frame: Vec<u8>) {
        self.rx.push_back(frame);
    }

    /// A client read: take the next received frame, if any.
    pub fn read_frame(&mut self) -> Option<Vec<u8>> {
        self.rx.pop_front()
    }

    /// A client write: enqueue a frame to transmit.
    pub fn write_frame(&mut self, frame: Vec<u8>) {
        self.tx.push_back(frame);
    }

    /// Scheme → driver: the next frame to transmit (`VirtioNet::transmit`).
    pub fn take_tx(&mut self) -> Option<Vec<u8>> {
        self.tx.pop_front()
    }

    /// Whether any client frames are waiting to be transmitted.
    pub fn has_tx(&self) -> bool {
        !self.tx.is_empty()
    }
}

/// Framebuffer scheme front-end. A client write requests a flip; the driver
/// drains [`FbScheme::take_flips`] and calls `VirtioGpu::present`.
#[derive(Default)]
pub struct FbScheme {
    flips_requested: usize,
}

impl FbScheme {
    /// A client requested a flip.
    pub fn request_flip(&mut self) {
        self.flips_requested += 1;
    }

    /// How many flips were requested since the last drain.
    pub fn take_flips(&mut self) -> usize {
        std::mem::take(&mut self.flips_requested)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_scheme_bridges_rx_and_tx() {
        let mut scheme = NetScheme::default();
        // Driver delivers a received frame; a reader pops it.
        scheme.deliver(vec![1, 2, 3]);
        assert_eq!(scheme.read_frame(), Some(vec![1, 2, 3]));
        assert_eq!(scheme.read_frame(), None);
        // A writer enqueues; the driver drains it to transmit.
        assert!(!scheme.has_tx());
        scheme.write_frame(vec![4, 5]);
        assert!(scheme.has_tx());
        assert_eq!(scheme.take_tx(), Some(vec![4, 5]));
        assert_eq!(scheme.take_tx(), None);
    }

    #[test]
    fn fb_scheme_counts_flip_requests() {
        let mut scheme = FbScheme::default();
        scheme.request_flip();
        scheme.request_flip();
        assert_eq!(scheme.take_flips(), 2);
        assert_eq!(scheme.take_flips(), 0); // drained
    }
}
