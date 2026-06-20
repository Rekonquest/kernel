//! A virtio-net orchestrator: policy on top of the virtqueue.
//!
//! This is the thin layer that turns the generic [`VirtQueue`] into a network
//! device. It owns three things and no mechanism of its own:
//!
//! - the **feature negotiation** (which virtio-net features to accept), via the
//!   kit's [`Features`] intersection;
//! - the **12-byte virtio-net header** prepended to every packet (virtio 1.0);
//! - the **RX/TX queue roles** — TX carries readable [header, frame] chains the
//!   device sends; RX carries device-writable buffers the device fills.
//!
//! Buffer memory stays with the caller (as DMA regions), exactly as the queue
//! does — a real driver layers a fixed buffer pool above this.
//!
//! [`VirtQueue`]: super::queue::VirtQueue
//! [`Features`]: crate::feature::Features

use core::ptr;

use crate::dma::DmaRegion;
use crate::feature::{self, Features};

use super::queue::{QueueError, Segment, Used, VirtQueue};

/// virtio-net feature bit indices (for [`Features::bit`]).
pub mod feature_bits {
    /// Device handles packets with a partial checksum.
    pub const CSUM: u32 = 0;
    /// Device has a given MAC address.
    pub const MAC: u32 = 5;
    /// Driver can merge receive buffers.
    pub const MRG_RXBUF: u32 = 15;
    /// Configuration status field is available.
    pub const STATUS: u32 = 16;
    /// Transport: virtio 1.0 (non-legacy). Mandatory here.
    pub const VERSION_1: u32 = 32;
}

/// The 12-byte virtio-net header prepended to every packet (virtio 1.0, which
/// always includes `num_buffers`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VirtioNetHdr {
    pub flags: u8,
    pub gso_type: u8,
    pub hdr_len: u16,
    pub gso_size: u16,
    pub csum_start: u16,
    pub csum_offset: u16,
    pub num_buffers: u16,
}

impl VirtioNetHdr {
    /// On-wire size in bytes.
    pub const LEN: usize = 12;

    /// Serialize the header (little-endian) into `dst`.
    ///
    /// # Safety
    /// `dst` must be valid and writable for at least [`LEN`](Self::LEN) bytes and
    /// 2-byte aligned.
    pub unsafe fn write_to(&self, dst: *mut u8) {
        unsafe {
            ptr::write_volatile(dst, self.flags);
            ptr::write_volatile(dst.add(1), self.gso_type);
            ptr::write_volatile(dst.add(2) as *mut u16, self.hdr_len.to_le());
            ptr::write_volatile(dst.add(4) as *mut u16, self.gso_size.to_le());
            ptr::write_volatile(dst.add(6) as *mut u16, self.csum_start.to_le());
            ptr::write_volatile(dst.add(8) as *mut u16, self.csum_offset.to_le());
            ptr::write_volatile(dst.add(10) as *mut u16, self.num_buffers.to_le());
        }
    }
}

/// Negotiate driver features against what the device `offered`.
///
/// Requires `VERSION_1`; additionally accepts `MAC` and `STATUS` if offered.
/// Returns the agreed set, or the missing mandatory bits on failure.
pub fn negotiate(offered: Features) -> Result<Features, Features> {
    let wanted = Features::bit(feature_bits::VERSION_1)
        .union(Features::bit(feature_bits::MAC))
        .union(Features::bit(feature_bits::STATUS));
    let required = Features::bit(feature_bits::VERSION_1);
    feature::negotiate_checked(offered, wanted, required)
}

/// A virtio-net device: a receive queue and a transmit queue plus the agreed
/// features. `RXQ`/`TXQ` are the queue sizes.
pub struct VirtioNet<const RXQ: usize, const TXQ: usize> {
    /// Receive queue (device-writable buffers).
    pub rx: VirtQueue<RXQ>,
    /// Transmit queue (driver-written [header, frame] chains).
    pub tx: VirtQueue<TXQ>,
    features: Features,
}

impl<const RXQ: usize, const TXQ: usize> VirtioNet<RXQ, TXQ> {
    /// Assemble the device from its two queues and the negotiated features.
    pub fn new(rx: VirtQueue<RXQ>, tx: VirtQueue<TXQ>, features: Features) -> Self {
        Self { rx, tx, features }
    }

    /// The feature set agreed with the device.
    pub fn features(&self) -> Features {
        self.features
    }

    /// Transmit a frame: write the net header into `hdr` and enqueue the
    /// readable chain `[header, frame]` on the TX queue. Returns the chain head.
    pub fn transmit<H: DmaRegion>(
        &mut self,
        hdr: &H,
        frame: Segment,
    ) -> Result<u16, QueueError> {
        if hdr.len() < VirtioNetHdr::LEN {
            return Err(QueueError::RegionTooSmall);
        }
        // SAFETY: checked the header region is large enough.
        unsafe { VirtioNetHdr::default().write_to(hdr.cpu_ptr()) };
        self.tx.add_buf(&[
            Segment {
                addr: hdr.phys_addr(),
                len: VirtioNetHdr::LEN as u32,
                device_writable: false,
            },
            frame,
        ])
    }

    /// Reap a completed transmit, freeing its descriptors.
    pub fn poll_tx(&mut self) -> Option<Used> {
        self.tx.poll_used()
    }

    /// Post a device-writable receive buffer (space for header + payload) so the
    /// device can deliver an incoming frame into it.
    pub fn post_rx(&mut self, buffer: Segment) -> Result<u16, QueueError> {
        self.rx.add_buf(&[Segment {
            device_writable: true,
            ..buffer
        }])
    }

    /// Reap a received frame. [`Used::len`] is the total bytes the device wrote
    /// (virtio-net header included).
    pub fn poll_rx(&mut self) -> Option<Used> {
        self.rx.poll_used()
    }
}

/// A pool of pre-posted receive buffers for a virtio-net RX queue.
///
/// The device can only deliver an incoming frame into a buffer the driver has
/// already handed it, so a NIC keeps the RX ring full at all times. `post_all`
/// fills it; `poll` returns each completed receive as the pool buffer index and
/// the bytes written (virtio-net header included); `repost` returns a drained
/// buffer to the device. Each buffer is one device-writable descriptor.
pub struct RxPool<const POOL: usize> {
    bufs: [RxBuf; POOL],
}

#[derive(Clone, Copy)]
struct RxBuf {
    phys: u64,
    len: u32,
    posted_head: Option<u16>,
}

impl<const POOL: usize> RxPool<POOL> {
    /// Build a pool from `POOL` receive buffers, each given as `(phys_addr, len)`.
    pub fn new(buffers: [(u64, u32); POOL]) -> Self {
        Self {
            bufs: buffers.map(|(phys, len)| RxBuf {
                phys,
                len,
                posted_head: None,
            }),
        }
    }

    /// Post every not-yet-posted buffer to the RX queue so the device can
    /// deliver frames into them.
    pub fn post_all<const Q: usize>(&mut self, vq: &mut VirtQueue<Q>) -> Result<(), QueueError> {
        for buf in self.bufs.iter_mut() {
            if buf.posted_head.is_none() {
                let head = vq.add_buf(&[Segment {
                    addr: buf.phys,
                    len: buf.len,
                    device_writable: true,
                }])?;
                buf.posted_head = Some(head);
            }
        }
        Ok(())
    }

    /// Reap one received frame: the pool buffer index it landed in and the bytes
    /// the device wrote. The buffer is left un-posted — call [`repost`] once the
    /// frame has been consumed.
    ///
    /// [`repost`]: RxPool::repost
    pub fn poll<const Q: usize>(&mut self, vq: &mut VirtQueue<Q>) -> Option<(usize, u32)> {
        let Used { head, len } = vq.poll_used()?;
        let index = self.bufs.iter().position(|b| b.posted_head == Some(head))?;
        self.bufs[index].posted_head = None;
        Some((index, len))
    }

    /// Re-post buffer `index` to the device after its frame has been consumed.
    pub fn repost<const Q: usize>(
        &mut self,
        vq: &mut VirtQueue<Q>,
        index: usize,
    ) -> Result<(), QueueError> {
        let Some(buf) = self.bufs.get_mut(index) else {
            return Ok(());
        };
        if buf.posted_head.is_some() {
            return Ok(());
        }
        let head = vq.add_buf(&[Segment {
            addr: buf.phys,
            len: buf.len,
            device_writable: true,
        }])?;
        buf.posted_head = Some(head);
        Ok(())
    }

    /// The `(phys_addr, len)` of buffer `index`.
    pub fn buffer(&self, index: usize) -> Option<(u64, u32)> {
        self.bufs.get(index).map(|b| (b.phys, b.len))
    }

    /// Number of buffers currently posted to the device.
    pub fn posted(&self) -> usize {
        self.bufs.iter().filter(|b| b.posted_head.is_some()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_twelve_bytes() {
        assert_eq!(VirtioNetHdr::LEN, 12);
    }

    #[test]
    fn negotiate_requires_version_1() {
        // Device without VERSION_1 -> the mandatory bit is reported missing.
        let legacy = Features::bit(feature_bits::MAC);
        assert_eq!(
            negotiate(legacy),
            Err(Features::bit(feature_bits::VERSION_1))
        );
    }

    #[test]
    fn negotiate_keeps_the_common_subset() {
        // Device offers VERSION_1 + MAC + CSUM; we want VERSION_1 + MAC + STATUS.
        let device = Features::bit(feature_bits::VERSION_1)
            .union(Features::bit(feature_bits::MAC))
            .union(Features::bit(feature_bits::CSUM));
        let agreed = negotiate(device).unwrap();
        assert!(agreed.contains(Features::bit(feature_bits::VERSION_1)));
        assert!(agreed.contains(Features::bit(feature_bits::MAC)));
        // CSUM wasn't requested, STATUS wasn't offered -> neither survives.
        assert!(!agreed.contains(Features::bit(feature_bits::CSUM)));
        assert!(!agreed.contains(Features::bit(feature_bits::STATUS)));
    }
}
