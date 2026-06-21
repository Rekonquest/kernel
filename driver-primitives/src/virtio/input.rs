//! virtio-input — keyboard / mouse / tablet events over the kit.
//!
//! The device delivers `virtio_input_event`s (type, code, value) into buffers
//! the driver pre-posts on the event queue — exactly the receive pattern the NIC
//! uses, so it reuses [`RxPool`] verbatim. The only input-specific bit is
//! parsing the 8-byte event.
//!
//! [`RxPool`]: super::net::RxPool

use core::ptr;

use crate::dma::DmaRegion;

use super::{
    net::RxPool,
    queue::{QueueError, VirtQueue},
};

/// Event types (`virtio_input_event.type`, matching Linux `EV_*`).
pub mod ev {
    pub const SYN: u16 = 0;
    pub const KEY: u16 = 1;
    pub const REL: u16 = 2;
    pub const ABS: u16 = 3;
}

/// On-wire size of one input event.
pub const EVENT_LEN: usize = 8;

/// A decoded input event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputEvent {
    pub kind: u16,
    pub code: u16,
    pub value: u32,
}

/// Parse a `virtio_input_event` from a buffer of at least [`EVENT_LEN`] bytes.
pub fn parse_event<B: DmaRegion>(buf: &B) -> InputEvent {
    let p = buf.cpu_ptr();
    // SAFETY: caller passes a buffer the device wrote >= EVENT_LEN bytes into.
    unsafe {
        InputEvent {
            kind: u16::from_le(ptr::read_volatile(p as *const u16)),
            code: u16::from_le(ptr::read_volatile(p.add(2) as *const u16)),
            value: u32::from_le(ptr::read_volatile(p.add(4) as *const u32)),
        }
    }
}

/// A virtio-input device: an event queue with a pool of `POOL` pre-posted
/// buffers the device fills with input events.
pub struct VirtioInput<const Q: usize, const POOL: usize> {
    /// The event queue (queue 0).
    pub eventq: VirtQueue<Q>,
    pool: RxPool<POOL>,
}

impl<const Q: usize, const POOL: usize> VirtioInput<Q, POOL> {
    /// Build from the event queue and `POOL` event buffers, each `(phys, len)`.
    pub fn new(eventq: VirtQueue<Q>, buffers: [(u64, u32); POOL]) -> Self {
        Self {
            eventq,
            pool: RxPool::new(buffers),
        }
    }

    /// Pre-post every event buffer so the device can deliver into them.
    pub fn start(&mut self) -> Result<(), QueueError> {
        self.pool.post_all(&mut self.eventq)
    }

    /// Poll for a delivered event buffer: `(pool buffer index, bytes written)`.
    /// Parse it with [`parse_event`], then [`repost`](Self::repost) the buffer.
    pub fn poll(&mut self) -> Option<(usize, u32)> {
        self.pool.poll(&mut self.eventq)
    }

    /// Re-post buffer `index` after the event has been consumed.
    pub fn repost(&mut self, index: usize) -> Result<(), QueueError> {
        self.pool.repost(&mut self.eventq, index)
    }

    /// The `(phys, len)` of pool buffer `index`.
    pub fn buffer(&self, index: usize) -> Option<(u64, u32)> {
        self.pool.buffer(index)
    }
}
