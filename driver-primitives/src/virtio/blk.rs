//! virtio-blk (storage) — block read/write over the kit's virtqueue.
//!
//! Each request is a three-descriptor chain: a readable header
//! (`virtio_blk_req`: type + sector), the data buffer (device-writable for a
//! read, driver-readable for a write), and a one-byte device-writable status.
//! The orchestrator serializes the header and chains the descriptors; the queue
//! and reactor do the rest.

use core::ptr;

use crate::dma::DmaRegion;

use super::queue::{QueueError, Segment, Used, VirtQueue};

/// Request types (`virtio_blk_req.type`).
pub mod req {
    /// Read from the device into a buffer.
    pub const IN: u32 = 0;
    /// Write a buffer to the device.
    pub const OUT: u32 = 1;
    /// Flush the device's cache.
    pub const FLUSH: u32 = 4;
}

/// Status byte values written by the device.
pub mod status {
    pub const OK: u8 = 0;
    pub const IOERR: u8 = 1;
    pub const UNSUPP: u8 = 2;
}

/// Length of the request header, in bytes.
pub const HDR_LEN: usize = 16;
/// Standard logical block size.
pub const SECTOR_SIZE: usize = 512;

/// A virtio-blk device: a single request queue.
pub struct VirtioBlk<const Q: usize> {
    /// The request queue.
    pub queue: VirtQueue<Q>,
}

impl<const Q: usize> VirtioBlk<Q> {
    /// Wrap the request queue.
    pub fn new(queue: VirtQueue<Q>) -> Self {
        Self { queue }
    }

    /// Read sectors starting at `sector` into `data` (device-writable). `hdr`
    /// (>= [`HDR_LEN`]) and a 1-byte `status` are the other two descriptors.
    pub fn read<H: DmaRegion, D: DmaRegion, S: DmaRegion>(
        &mut self,
        hdr: &H,
        data: &D,
        status: &S,
        sector: u64,
    ) -> Result<u16, QueueError> {
        self.request(hdr, data, status, sector, req::IN, true)
    }

    /// Write the contents of `data` (driver-readable) to sectors starting at
    /// `sector`.
    pub fn write<H: DmaRegion, D: DmaRegion, S: DmaRegion>(
        &mut self,
        hdr: &H,
        data: &D,
        status: &S,
        sector: u64,
    ) -> Result<u16, QueueError> {
        self.request(hdr, data, status, sector, req::OUT, false)
    }

    fn request<H: DmaRegion, D: DmaRegion, S: DmaRegion>(
        &mut self,
        hdr: &H,
        data: &D,
        status: &S,
        sector: u64,
        kind: u32,
        data_device_writable: bool,
    ) -> Result<u16, QueueError> {
        if hdr.len() < HDR_LEN || status.is_empty() {
            return Err(QueueError::RegionTooSmall);
        }
        // SAFETY: hdr is at least HDR_LEN bytes (checked) and DMA-valid.
        unsafe {
            let p = hdr.cpu_ptr();
            ptr::write_volatile(p as *mut u32, kind.to_le());
            ptr::write_volatile(p.add(4) as *mut u32, 0u32.to_le()); // reserved
            ptr::write_volatile(p.add(8) as *mut u64, sector.to_le());
        }
        self.queue.add_buf(&[
            Segment {
                addr: hdr.phys_addr(),
                len: HDR_LEN as u32,
                device_writable: false,
            },
            Segment {
                addr: data.phys_addr(),
                len: data.len() as u32,
                device_writable: data_device_writable,
            },
            Segment {
                addr: status.phys_addr(),
                len: 1,
                device_writable: true,
            },
        ])
    }

    /// Reap a completed request.
    pub fn poll(&mut self) -> Option<Used> {
        self.queue.poll_used()
    }

    /// Read the device's status byte from a completed request's status buffer.
    pub fn status_of<S: DmaRegion>(status: &S) -> u8 {
        // SAFETY: a status region is always at least one byte.
        unsafe { ptr::read_volatile(status.cpu_ptr()) }
    }
}
