//! A faithful virtio split virtqueue, built on the kit's DMA seam.
//!
//! This is where the kit's *concepts* meet virtio's *concrete ABI*. A real
//! device reads and writes three structures in DMA memory with exact layouts
//! and little-endian fields, so the queue cannot use the abstract [`Ring`] — it
//! lays out the real thing:
//!
//! - **descriptor table** — `Q` × 16-byte `virtq_desc { addr, len, flags, next }`
//! - **available ring** (driver → device) — `flags, idx, ring[Q]`
//! - **used ring** (device → driver) — `flags, idx, ring[Q] of { id, len }`
//!
//! The driver allocates descriptors from a private free list, chains a buffer's
//! segments, publishes the chain head into the available ring, and bumps
//! `avail.idx`; the device consumes from there and posts completions into the
//! used ring. The memory barriers and LE encoding are the parts that make this
//! correct on real hardware, and they are present here.
//!
//! The same `add_buf` / `poll_used` cycle is the kit's submit/reap pattern — the
//! [`Reactor`] sits naturally on top, with this queue as its executor.
//!
//! [`Ring`]: crate::ring::Ring
//! [`Reactor`]: crate::reactor::Reactor

use core::{
    ptr,
    sync::atomic::{fence, Ordering},
};

use crate::dma::{Descriptor, DmaRegion};

/// Maximum number of segments in a single buffer chain.
pub const MAX_CHAIN: usize = 16;

/// One contiguous buffer in a descriptor chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// Device-visible (physical) address of the buffer.
    pub addr: u64,
    /// Length in bytes.
    pub len: u32,
    /// If true the device writes this buffer (driver reads it); otherwise the
    /// device reads it (driver wrote it).
    pub device_writable: bool,
}

/// A completed buffer, reported by the device in the used ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Used {
    /// Head descriptor index of the completed chain.
    pub head: u16,
    /// Bytes written by the device (for device-writable buffers).
    pub len: u32,
}

/// What can go wrong adding a buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueError {
    /// The backing DMA region is smaller than [`VirtQueue::required_bytes`].
    RegionTooSmall,
    /// A chain with no segments was requested.
    EmptyRequest,
    /// The chain exceeds [`MAX_CHAIN`] segments.
    ChainTooLong,
    /// Not enough free descriptors for the requested chain.
    Full,
}

/// A split virtqueue of compile-time size `Q` over a caller-supplied DMA region.
///
/// `Q` must be a power of two ≤ 32768 (the virtio constraint). Holds raw
/// pointers into the DMA region, so it is neither `Send` nor `Sync`: drive each
/// queue from a single owner.
pub struct VirtQueue<const Q: usize> {
    desc: *mut u8,
    avail: *mut u8,
    used: *mut u8,
    desc_phys: u64,
    avail_phys: u64,
    used_phys: u64,
    /// Stack of free descriptor indices.
    free: [u16; Q],
    num_free: u16,
    /// Driver-owned shadow of `avail.idx`.
    avail_idx: u16,
    /// Last `used.idx` the driver has consumed.
    last_used: u16,
}

impl<const Q: usize> VirtQueue<Q> {
    /// Bytes occupied by the descriptor table.
    pub const DESC_BYTES: usize = 16 * Q;
    /// Bytes occupied by the available ring (incl. the optional `used_event`).
    pub const AVAIL_BYTES: usize = 6 + 2 * Q;
    /// Bytes occupied by the used ring (incl. the optional `avail_event`).
    pub const USED_BYTES: usize = 6 + 8 * Q;
    /// Offset of the available ring within the backing region.
    pub const AVAIL_OFFSET: usize = Self::DESC_BYTES;
    /// Offset of the used ring (4-byte aligned).
    pub const USED_OFFSET: usize = (Self::AVAIL_OFFSET + Self::AVAIL_BYTES + 3) & !3;

    /// Total bytes the backing DMA region must provide.
    pub const fn required_bytes() -> usize {
        Self::USED_OFFSET + Self::USED_BYTES
    }

    /// Lay a virtqueue over `region`.
    ///
    /// # Safety
    /// `region` must stay alive, mapped, and DMA-coherent for the lifetime of
    /// the queue, and must be at least 16-byte aligned. The region is zeroed.
    pub unsafe fn new<R: DmaRegion>(region: &R) -> Result<Self, QueueError> {
        if region.len() < Self::required_bytes() {
            return Err(QueueError::RegionTooSmall);
        }
        let base = region.cpu_ptr();
        let phys = region.phys_addr();
        // SAFETY: region is large enough (checked) and the caller guarantees it
        // is valid for the whole range.
        unsafe { ptr::write_bytes(base, 0, Self::required_bytes()) };

        let mut free = [0u16; Q];
        let mut i = 0;
        while i < Q {
            free[i] = i as u16;
            i += 1;
        }

        Ok(Self {
            desc: base,
            avail: unsafe { base.add(Self::AVAIL_OFFSET) },
            used: unsafe { base.add(Self::USED_OFFSET) },
            desc_phys: phys,
            avail_phys: phys + Self::AVAIL_OFFSET as u64,
            used_phys: phys + Self::USED_OFFSET as u64,
            free,
            num_free: Q as u16,
            avail_idx: 0,
            last_used: 0,
        })
    }

    /// Queue size.
    pub const fn size(&self) -> u16 {
        Q as u16
    }

    /// Free descriptors remaining.
    pub const fn num_free(&self) -> u16 {
        self.num_free
    }

    /// Physical address of the descriptor table (to program into the device).
    pub const fn desc_addr(&self) -> u64 {
        self.desc_phys
    }
    /// Physical address of the available ring.
    pub const fn avail_addr(&self) -> u64 {
        self.avail_phys
    }
    /// Physical address of the used ring.
    pub const fn used_addr(&self) -> u64 {
        self.used_phys
    }

    fn alloc_desc(&mut self) -> Option<u16> {
        if self.num_free == 0 {
            return None;
        }
        self.num_free -= 1;
        Some(self.free[self.num_free as usize])
    }

    fn free_desc(&mut self, idx: u16) {
        self.free[self.num_free as usize] = idx;
        self.num_free += 1;
    }

    /// Add a buffer chain and publish it to the device. Returns the chain's head
    /// descriptor index, which the device echoes back in [`Used::head`].
    pub fn add_buf(&mut self, segments: &[Segment]) -> Result<u16, QueueError> {
        let n = segments.len();
        if n == 0 {
            return Err(QueueError::EmptyRequest);
        }
        if n > MAX_CHAIN {
            return Err(QueueError::ChainTooLong);
        }
        if n > self.num_free as usize {
            return Err(QueueError::Full);
        }

        // Reserve every descriptor up front so a failure cannot half-build a chain.
        let mut idxs = [0u16; MAX_CHAIN];
        for slot in idxs.iter_mut().take(n) {
            *slot = self.alloc_desc().expect("checked num_free >= n");
        }

        for k in 0..n {
            let seg = &segments[k];
            let mut flags = 0u16;
            if seg.device_writable {
                flags |= Descriptor::F_WRITE;
            }
            let next = if k + 1 < n {
                flags |= Descriptor::F_NEXT;
                idxs[k + 1]
            } else {
                0
            };
            // SAFETY: idxs[k] < Q, so the descriptor slot is in-range.
            unsafe { self.write_desc(idxs[k], seg.addr, seg.len, flags, next) };
        }

        let head = idxs[0];
        let slot = self.avail_idx % (Q as u16);
        // SAFETY: slot < Q.
        unsafe { self.write_avail_ring(slot, head) };
        // Ensure the ring entry and descriptors are visible before idx bumps.
        fence(Ordering::Release);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        // SAFETY: avail layout.
        unsafe { self.write_avail_idx(self.avail_idx) };

        Ok(head)
    }

    /// Reap one completed chain from the used ring, freeing its descriptors.
    /// Returns `None` when the device has posted nothing new.
    pub fn poll_used(&mut self) -> Option<Used> {
        // SAFETY: used layout.
        let used_idx = unsafe { self.read_used_idx() };
        if used_idx == self.last_used {
            return None;
        }
        // See the device's writes to the used element before reading it.
        fence(Ordering::Acquire);

        let slot = self.last_used % (Q as u16);
        // SAFETY: slot < Q.
        let (id, len) = unsafe { self.read_used_elem(slot) };
        self.last_used = self.last_used.wrapping_add(1);

        // Free the whole descriptor chain back to the free list.
        let mut idx = id as u16;
        loop {
            // SAFETY: idx came from a chain we built; it is < Q.
            let flags = unsafe { self.read_desc_flags(idx) };
            let next = unsafe { self.read_desc_next(idx) };
            self.free_desc(idx);
            if flags & Descriptor::F_NEXT == 0 {
                break;
            }
            idx = next;
        }

        Some(Used {
            head: id as u16,
            len,
        })
    }

    // --- raw, little-endian, volatile field accessors ------------------------

    unsafe fn write_desc(&self, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let p = unsafe { self.desc.add(i as usize * 16) };
        unsafe {
            ptr::write_volatile(p as *mut u64, addr.to_le());
            ptr::write_volatile(p.add(8) as *mut u32, len.to_le());
            ptr::write_volatile(p.add(12) as *mut u16, flags.to_le());
            ptr::write_volatile(p.add(14) as *mut u16, next.to_le());
        }
    }

    unsafe fn read_desc_flags(&self, i: u16) -> u16 {
        u16::from_le(unsafe {
            ptr::read_volatile(self.desc.add(i as usize * 16 + 12) as *const u16)
        })
    }

    unsafe fn read_desc_next(&self, i: u16) -> u16 {
        u16::from_le(unsafe {
            ptr::read_volatile(self.desc.add(i as usize * 16 + 14) as *const u16)
        })
    }

    unsafe fn write_avail_ring(&self, slot: u16, head: u16) {
        let p = unsafe { self.avail.add(4 + slot as usize * 2) };
        unsafe { ptr::write_volatile(p as *mut u16, head.to_le()) };
    }

    unsafe fn write_avail_idx(&self, idx: u16) {
        unsafe { ptr::write_volatile(self.avail.add(2) as *mut u16, idx.to_le()) };
    }

    unsafe fn read_used_idx(&self) -> u16 {
        u16::from_le(unsafe { ptr::read_volatile(self.used.add(2) as *const u16) })
    }

    unsafe fn read_used_elem(&self, slot: u16) -> (u32, u32) {
        let p = unsafe { self.used.add(4 + slot as usize * 8) };
        let id = u32::from_le(unsafe { ptr::read_volatile(p as *const u32) });
        let len = u32::from_le(unsafe { ptr::read_volatile(p.add(4) as *const u32) });
        (id, len)
    }
}
