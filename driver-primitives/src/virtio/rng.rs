//! virtio-rng (entropy) — the smallest virtio driver there is, pure reuse.
//!
//! One queue. The driver hands the device a writable buffer; the device fills it
//! with random bytes and completes it. Nothing here is rng-specific except the
//! intent — it is just [`VirtQueue`] with a device-writable descriptor.
//!
//! [`VirtQueue`]: super::queue::VirtQueue

use crate::dma::DmaRegion;

use super::queue::{QueueError, Segment, Used, VirtQueue};

/// A virtio-entropy device: a single request queue.
pub struct VirtioRng<const Q: usize> {
    /// The entropy request queue.
    pub queue: VirtQueue<Q>,
}

impl<const Q: usize> VirtioRng<Q> {
    /// Wrap the request queue.
    pub fn new(queue: VirtQueue<Q>) -> Self {
        Self { queue }
    }

    /// Request entropy: give the device a writable buffer to fill. Returns the
    /// queue head; the completion's [`Used::len`] is how many bytes it wrote.
    pub fn request<B: DmaRegion>(&mut self, buf: &B) -> Result<u16, QueueError> {
        self.queue.add_buf(&[Segment {
            addr: buf.phys_addr(),
            len: buf.len() as u32,
            device_writable: true,
        }])
    }

    /// Reap a completed entropy request.
    pub fn poll(&mut self) -> Option<Used> {
        self.queue.poll_used()
    }
}
