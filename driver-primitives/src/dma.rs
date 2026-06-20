//! DMA regions and descriptors — handing device-visible memory to hardware.
//!
//! A device reads and writes physical memory directly, so a driver must give it
//! buffers that are (a) CPU-visible to fill/drain and (b) physically addressable
//! by the device. [`DmaRegion`] is that seam — and it is deliberately a *trait*,
//! not a concrete type, so the kit stays portable: the OS supplies the region.
//! On Redox that is `redox_syscall::Dma<T>` (its physical address and CPU
//! mapping satisfy this contract); elsewhere it is whatever the platform's
//! coherent allocator returns.
//!
//! [`Descriptor`] is the device-visible "here is a buffer" record shared by
//! every descriptor-ring device (virtio, NVMe, USB, GPU). Its flag values match
//! virtio's so the same descriptors drive a real virtqueue unchanged.

/// A chunk of DMA-capable memory, addressable by both the CPU and the device.
pub trait DmaRegion {
    /// CPU-visible pointer to the region (for the driver to fill/drain).
    fn cpu_ptr(&self) -> *mut u8;
    /// Device-visible (physical / bus) address of the region.
    fn phys_addr(&self) -> u64;
    /// Length of the region in bytes.
    fn len(&self) -> usize;
    /// Whether the region is zero-length.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A device-visible buffer descriptor. Flag values match virtio's
/// `VIRTQ_DESC_F_*` so these descriptors drive a real virtqueue directly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Descriptor {
    /// Device-visible address of the buffer.
    pub addr: u64,
    /// Length in bytes.
    pub len: u32,
    /// Descriptor flags (see the `F_*` constants).
    pub flags: u16,
}

impl Descriptor {
    /// This buffer chains to the next descriptor (`VIRTQ_DESC_F_NEXT`).
    pub const F_NEXT: u16 = 1;
    /// The device writes this buffer; the driver reads it (`VIRTQ_DESC_F_WRITE`).
    pub const F_WRITE: u16 = 2;
    /// This buffer is itself a table of descriptors (`VIRTQ_DESC_F_INDIRECT`).
    pub const F_INDIRECT: u16 = 4;

    /// A descriptor with explicit fields.
    pub const fn new(addr: u64, len: u32, flags: u16) -> Self {
        Self { addr, len, flags }
    }

    /// A descriptor pointing at an entire DMA region.
    pub fn for_region(region: &impl DmaRegion, flags: u16) -> Self {
        Self {
            addr: region.phys_addr(),
            len: region.len() as u32,
            flags,
        }
    }

    /// Whether this descriptor chains to another.
    pub const fn has_next(self) -> bool {
        self.flags & Self::F_NEXT != 0
    }

    /// Whether the device writes this buffer (vs. reads it).
    pub const fn is_device_writable(self) -> bool {
        self.flags & Self::F_WRITE != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeRegion {
        buf: Vec<u8>,
        phys: u64,
    }

    impl DmaRegion for FakeRegion {
        fn cpu_ptr(&self) -> *mut u8 {
            self.buf.as_ptr() as *mut u8
        }
        fn phys_addr(&self) -> u64 {
            self.phys
        }
        fn len(&self) -> usize {
            self.buf.len()
        }
    }

    #[test]
    fn descriptor_describes_a_region() {
        let region = FakeRegion {
            buf: vec![0u8; 128],
            phys: 0x4000,
        };
        let desc = Descriptor::for_region(&region, Descriptor::F_WRITE);
        assert_eq!(desc.addr, 0x4000);
        assert_eq!(desc.len, 128);
        assert!(desc.is_device_writable());
        assert!(!desc.has_next());
        assert!(!region.is_empty());
    }

    #[test]
    fn flag_helpers_match_virtio_values() {
        assert_eq!(Descriptor::F_NEXT, 1);
        assert_eq!(Descriptor::F_WRITE, 2);
        assert_eq!(Descriptor::F_INDIRECT, 4);
        let chained = Descriptor::new(0, 0, Descriptor::F_NEXT);
        assert!(chained.has_next());
        assert!(!chained.is_device_writable());
    }
}
