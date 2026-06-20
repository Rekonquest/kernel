//! A virtio-gpu 2D orchestrator — the device that drives real (virtual) monitors.
//!
//! virtio-gpu (device id 16) is what QEMU exposes to put pixels on screens. It
//! reuses the kit's [`VirtQueue`] for its control queue and the
//! [`transport`](super::transport) for bring-up; this module adds the GPU 2D
//! command vocabulary and serializes it into the exact little-endian wire
//! structures the device expects.
//!
//! The 2D present flow for one scanout (monitor):
//! 1. `get_display_info` — discover scanouts and their preferred rects;
//! 2. `create_resource_2d` — a host-side framebuffer resource;
//! 3. `attach_backing` — point the resource at guest DMA memory;
//! 4. `set_scanout` — bind the resource to a scanout (monitor);
//! 5. draw into the backing, then `transfer_to_host_2d` + `resource_flush`.
//!
//! `get_display_info` is the bridge to [`Display`](crate::display::Display):
//! each enabled scanout becomes an output to configure with an atomic modeset.
//!
//! [`VirtQueue`]: super::queue::VirtQueue

use crate::dma::DmaRegion;

use super::queue::{QueueError, Segment, Used, VirtQueue};

/// Maximum scanouts (monitors) a virtio-gpu device reports.
pub const MAX_SCANOUTS: usize = 16;

/// Length of the common control header, in bytes.
pub const CTRL_HDR_LEN: usize = 24;
/// Length of one display descriptor in a display-info response.
pub const DISPLAY_ONE_LEN: usize = 24;
/// Length of a full display-info response.
pub const RESP_DISPLAY_INFO_LEN: usize = CTRL_HDR_LEN + MAX_SCANOUTS * DISPLAY_ONE_LEN;
/// Length of a no-data response (just the header).
pub const RESP_NODATA_LEN: usize = CTRL_HDR_LEN;
/// A command buffer of this size holds any 2D command here.
pub const CMD_MAX_LEN: usize = 64;

/// Control command/response types.
pub mod ty {
    pub const GET_DISPLAY_INFO: u32 = 0x0100;
    pub const RESOURCE_CREATE_2D: u32 = 0x0101;
    pub const RESOURCE_UNREF: u32 = 0x0102;
    pub const SET_SCANOUT: u32 = 0x0103;
    pub const RESOURCE_FLUSH: u32 = 0x0104;
    pub const TRANSFER_TO_HOST_2D: u32 = 0x0105;
    pub const RESOURCE_ATTACH_BACKING: u32 = 0x0106;

    pub const RESP_OK_NODATA: u32 = 0x1100;
    pub const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
}

/// A pixel format (the common one QEMU's stdvga uses).
pub mod format {
    pub const B8G8R8A8_UNORM: u32 = 1;
}

/// A rectangle, as virtio-gpu encodes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }
}

/// One scanout's state from a display-info response.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanoutInfo {
    pub rect: Rect,
    pub enabled: bool,
}

/// A little-endian cursor writer over a DMA buffer.
struct Writer {
    base: *mut u8,
    off: usize,
}

impl Writer {
    /// # Safety: `base` valid for at least the bytes written; offsets stay aligned.
    unsafe fn new(base: *mut u8) -> Self {
        Self { base, off: 0 }
    }
    unsafe fn u32(&mut self, v: u32) {
        unsafe { core::ptr::write_volatile(self.base.add(self.off) as *mut u32, v.to_le()) };
        self.off += 4;
    }
    unsafe fn u64(&mut self, v: u64) {
        unsafe { core::ptr::write_volatile(self.base.add(self.off) as *mut u64, v.to_le()) };
        self.off += 8;
    }
    unsafe fn hdr(&mut self, cmd_type: u32) {
        unsafe {
            self.u32(cmd_type); // type
            self.u32(0); // flags
            self.u64(0); // fence_id
            self.u32(0); // ctx_id
            self.u32(0); // padding
        }
    }
    unsafe fn rect(&mut self, r: Rect) {
        unsafe {
            self.u32(r.x);
            self.u32(r.y);
            self.u32(r.width);
            self.u32(r.height);
        }
    }
}

unsafe fn read_u32_le(base: *const u8, off: usize) -> u32 {
    u32::from_le(unsafe { core::ptr::read_volatile(base.add(off) as *const u32) })
}

/// The response type word at the start of a response buffer.
pub fn response_type(resp: &impl DmaRegion) -> u32 {
    // SAFETY: DmaRegion guarantees cpu_ptr is valid for len >= 4 bytes here.
    unsafe { read_u32_le(resp.cpu_ptr(), 0) }
}

/// Parse a display-info response into per-scanout state and the enabled count.
pub fn parse_display_info(resp: &impl DmaRegion) -> ([ScanoutInfo; MAX_SCANOUTS], usize) {
    let mut out = [ScanoutInfo::default(); MAX_SCANOUTS];
    let mut enabled_count = 0;
    if resp.len() < RESP_DISPLAY_INFO_LEN {
        return (out, 0);
    }
    let base = resp.cpu_ptr() as *const u8;
    for (i, slot) in out.iter_mut().enumerate() {
        let off = CTRL_HDR_LEN + i * DISPLAY_ONE_LEN;
        // SAFETY: length checked above.
        let rect = unsafe {
            Rect::new(
                read_u32_le(base, off),
                read_u32_le(base, off + 4),
                read_u32_le(base, off + 8),
                read_u32_le(base, off + 12),
            )
        };
        let enabled = unsafe { read_u32_le(base, off + 16) } != 0;
        if enabled {
            enabled_count += 1;
        }
        *slot = ScanoutInfo { rect, enabled };
    }
    (out, enabled_count)
}

/// A virtio-gpu device: a control queue carrying 2D commands.
pub struct VirtioGpu<const Q: usize> {
    /// The control queue (queue 0).
    pub control: VirtQueue<Q>,
}

impl<const Q: usize> VirtioGpu<Q> {
    /// Wrap a control virtqueue.
    pub fn new(control: VirtQueue<Q>) -> Self {
        Self { control }
    }

    /// Reap a completed control command.
    pub fn poll(&mut self) -> Option<Used> {
        self.control.poll_used()
    }

    /// Enqueue a command: a readable command buffer of `cmd_len` bytes followed
    /// by a device-writable response buffer.
    fn submit<C: DmaRegion, R: DmaRegion>(
        &mut self,
        cmd: &C,
        cmd_len: usize,
        resp: &R,
    ) -> Result<u16, QueueError> {
        self.control.add_buf(&[
            Segment {
                addr: cmd.phys_addr(),
                len: cmd_len as u32,
                device_writable: false,
            },
            Segment {
                addr: resp.phys_addr(),
                len: resp.len() as u32,
                device_writable: true,
            },
        ])
    }

    /// Ask the device which scanouts (monitors) exist.
    pub fn get_display_info<C: DmaRegion, R: DmaRegion>(
        &mut self,
        cmd: &C,
        resp: &R,
    ) -> Result<u16, QueueError> {
        if cmd.len() < CTRL_HDR_LEN || resp.len() < RESP_DISPLAY_INFO_LEN {
            return Err(QueueError::RegionTooSmall);
        }
        // SAFETY: cmd is large enough (checked) and DMA-valid (DmaRegion).
        let len = unsafe {
            let mut w = Writer::new(cmd.cpu_ptr());
            w.hdr(ty::GET_DISPLAY_INFO);
            w.off
        };
        self.submit(cmd, len, resp)
    }

    /// Create a 2D host resource (a framebuffer the device scans out).
    pub fn create_resource_2d<C: DmaRegion, R: DmaRegion>(
        &mut self,
        cmd: &C,
        resp: &R,
        resource_id: u32,
        fmt: u32,
        width: u32,
        height: u32,
    ) -> Result<u16, QueueError> {
        if cmd.len() < CMD_MAX_LEN {
            return Err(QueueError::RegionTooSmall);
        }
        let len = unsafe {
            let mut w = Writer::new(cmd.cpu_ptr());
            w.hdr(ty::RESOURCE_CREATE_2D);
            w.u32(resource_id);
            w.u32(fmt);
            w.u32(width);
            w.u32(height);
            w.off
        };
        self.submit(cmd, len, resp)
    }

    /// Attach a single backing region (guest DMA memory) to a resource.
    pub fn attach_backing<C: DmaRegion, R: DmaRegion>(
        &mut self,
        cmd: &C,
        resp: &R,
        resource_id: u32,
        backing_addr: u64,
        backing_len: u32,
    ) -> Result<u16, QueueError> {
        if cmd.len() < CMD_MAX_LEN {
            return Err(QueueError::RegionTooSmall);
        }
        let len = unsafe {
            let mut w = Writer::new(cmd.cpu_ptr());
            w.hdr(ty::RESOURCE_ATTACH_BACKING);
            w.u32(resource_id);
            w.u32(1); // nr_entries
            w.u64(backing_addr); // mem_entry.addr
            w.u32(backing_len); // mem_entry.length
            w.u32(0); // mem_entry.padding
            w.off
        };
        self.submit(cmd, len, resp)
    }

    /// Bind a resource to a scanout (monitor) over a rectangle.
    pub fn set_scanout<C: DmaRegion, R: DmaRegion>(
        &mut self,
        cmd: &C,
        resp: &R,
        scanout_id: u32,
        resource_id: u32,
        rect: Rect,
    ) -> Result<u16, QueueError> {
        if cmd.len() < CMD_MAX_LEN {
            return Err(QueueError::RegionTooSmall);
        }
        let len = unsafe {
            let mut w = Writer::new(cmd.cpu_ptr());
            w.hdr(ty::SET_SCANOUT);
            w.rect(rect);
            w.u32(scanout_id);
            w.u32(resource_id);
            w.off
        };
        self.submit(cmd, len, resp)
    }

    /// Copy a rectangle from the backing into the host resource.
    pub fn transfer_to_host_2d<C: DmaRegion, R: DmaRegion>(
        &mut self,
        cmd: &C,
        resp: &R,
        resource_id: u32,
        rect: Rect,
        offset: u64,
    ) -> Result<u16, QueueError> {
        if cmd.len() < CMD_MAX_LEN {
            return Err(QueueError::RegionTooSmall);
        }
        let len = unsafe {
            let mut w = Writer::new(cmd.cpu_ptr());
            w.hdr(ty::TRANSFER_TO_HOST_2D);
            w.rect(rect);
            w.u64(offset);
            w.u32(resource_id);
            w.u32(0); // padding
            w.off
        };
        self.submit(cmd, len, resp)
    }

    /// Present an updated rectangle: copy it from the backing into the host
    /// resource (`transfer_to_host_2d`) and then flush it to the scanout
    /// (`resource_flush`). The two commands pipeline, so each needs its own
    /// command/response buffer pair. Returns `(transfer_head, flush_head)`.
    #[allow(clippy::too_many_arguments)]
    pub fn present<CA, RA, CB, RB>(
        &mut self,
        transfer_cmd: &CA,
        transfer_resp: &RA,
        flush_cmd: &CB,
        flush_resp: &RB,
        resource_id: u32,
        rect: Rect,
        offset: u64,
    ) -> Result<(u16, u16), QueueError>
    where
        CA: DmaRegion,
        RA: DmaRegion,
        CB: DmaRegion,
        RB: DmaRegion,
    {
        let transfer = self.transfer_to_host_2d(transfer_cmd, transfer_resp, resource_id, rect, offset)?;
        let flush = self.resource_flush(flush_cmd, flush_resp, resource_id, rect)?;
        Ok((transfer, flush))
    }

    /// Present a rectangle of the resource to its scanout.
    pub fn resource_flush<C: DmaRegion, R: DmaRegion>(
        &mut self,
        cmd: &C,
        resp: &R,
        resource_id: u32,
        rect: Rect,
    ) -> Result<u16, QueueError> {
        if cmd.len() < CMD_MAX_LEN {
            return Err(QueueError::RegionTooSmall);
        }
        let len = unsafe {
            let mut w = Writer::new(cmd.cpu_ptr());
            w.hdr(ty::RESOURCE_FLUSH);
            w.rect(rect);
            w.u32(resource_id);
            w.u32(0); // padding
            w.off
        };
        self.submit(cmd, len, resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Buf {
        mem: Vec<u128>,
        len: usize,
    }
    impl Buf {
        fn new(bytes: usize) -> Self {
            let units = bytes.div_ceil(16).max(1);
            Buf { mem: vec![0u128; units], len: units * 16 }
        }
    }
    impl DmaRegion for Buf {
        fn cpu_ptr(&self) -> *mut u8 {
            self.mem.as_ptr() as *mut u8
        }
        fn phys_addr(&self) -> u64 {
            self.mem.as_ptr() as u64
        }
        fn len(&self) -> usize {
            self.len
        }
    }

    fn rd(buf: &Buf, off: usize) -> u32 {
        unsafe { read_u32_le(buf.cpu_ptr(), off) }
    }

    #[test]
    fn create_2d_serializes_correctly() {
        let region = Buf::new(VirtQueue::<16>::required_bytes());
        let mut gpu: VirtioGpu<16> = VirtioGpu::new(unsafe { VirtQueue::new(&region).unwrap() });
        let cmd = Buf::new(CMD_MAX_LEN);
        let resp = Buf::new(RESP_NODATA_LEN);
        gpu.create_resource_2d(&cmd, &resp, 7, format::B8G8R8A8_UNORM, 1920, 1080)
            .unwrap();
        assert_eq!(rd(&cmd, 0), ty::RESOURCE_CREATE_2D);
        assert_eq!(rd(&cmd, 24), 7); // resource_id
        assert_eq!(rd(&cmd, 28), format::B8G8R8A8_UNORM);
        assert_eq!(rd(&cmd, 32), 1920); // width
        assert_eq!(rd(&cmd, 36), 1080); // height
    }

    #[test]
    fn set_scanout_serializes_correctly() {
        let region = Buf::new(VirtQueue::<16>::required_bytes());
        let mut gpu: VirtioGpu<16> = VirtioGpu::new(unsafe { VirtQueue::new(&region).unwrap() });
        let cmd = Buf::new(CMD_MAX_LEN);
        let resp = Buf::new(RESP_NODATA_LEN);
        gpu.set_scanout(&cmd, &resp, 1, 7, Rect::new(0, 0, 1280, 720))
            .unwrap();
        assert_eq!(rd(&cmd, 0), ty::SET_SCANOUT);
        assert_eq!(rd(&cmd, 24), 0); // rect.x
        assert_eq!(rd(&cmd, 32), 1280); // rect.width
        assert_eq!(rd(&cmd, 36), 720); // rect.height
        assert_eq!(rd(&cmd, 40), 1); // scanout_id
        assert_eq!(rd(&cmd, 44), 7); // resource_id
    }
}
