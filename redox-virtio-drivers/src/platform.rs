//! The Redox binding of the kit's [`Platform`] seam.
//!
//! On Redox this maps device memory with `physmap`, allocates DMA-coherent
//! buffers with `physalloc`/`physmap`, and waits on the device interrupt through
//! the `irq` scheme (read blocks until the IRQ fires; write acknowledges). On
//! any other host it compiles to a stub that reports [`PlatformError::Unsupported`],
//! so the daemon logic still type-checks off-target.

use driver_primitives::{
    dma::DmaRegion,
    mmio::Bank,
    runtime::{Platform, PlatformError},
};

#[cfg(target_os = "redox")]
mod imp {
    use super::*;
    use syscall::{
        close,
        data::Map,
        flag::{MapFlags, O_CLOEXEC, O_RDWR},
        fmap, fpath, funmap, openat, read, write,
    };

    // Base fd for opening absolute, scheme-qualified paths (e.g. "irq:9"). The
    // exact convention is libredox's `open`; this uses the raw `openat` with a
    // sentinel base. Boot-verify against the running system.
    const ROOT_FD: usize = !0;

    fn map_flags() -> MapFlags {
        MapFlags::PROT_READ | MapFlags::PROT_WRITE | MapFlags::MAP_SHARED
    }

    /// Extract the physical base from a `memory:physical` scheme path. Accepts a
    /// hex base, optionally `0x`-prefixed and optionally a `<start>-<end>` range.
    /// The exact path format is boot-verified.
    fn parse_phys(path: &[u8]) -> Option<usize> {
        let s = core::str::from_utf8(path).ok()?.trim();
        let last = s.rsplit(['/', '=', '@', ':']).next()?;
        let start = last.split('-').next()?.trim();
        let start = start.strip_prefix("0x").unwrap_or(start);
        usize::from_str_radix(start, 16).ok()
    }

    /// DMA-coherent memory: a physical allocation from the `memory:physical`
    /// scheme, mapped for the CPU. `phys` is the device-visible base.
    pub struct RedoxDma {
        virt: *mut u8,
        phys: usize,
        len: usize,
        scheme_fd: usize,
    }

    impl RedoxDma {
        pub fn new(len: usize) -> Result<Self, PlatformError> {
            // Allocate device-visible (physical) memory and read back its
            // physical base, so phys_addr() reports what the device must DMA to —
            // not the userspace virtual mapping. On any path mismatch we fail
            // rather than hand out an address the device would corrupt.
            let scheme_fd = openat(ROOT_FD, "memory:physical", O_RDWR | O_CLOEXEC, 0)
                .map_err(|_| PlatformError::OutOfMemory)?;
            let map = Map {
                offset: 0,
                size: len,
                flags: map_flags(),
                address: 0,
            };
            let virt = unsafe { fmap(scheme_fd, &map) }.map_err(|_| PlatformError::MapFailed)?;

            let mut path = [0u8; 256];
            let n = fpath(scheme_fd, &mut path).map_err(|_| PlatformError::MapFailed)?;
            let phys = parse_phys(&path[..n.min(path.len())]).ok_or(PlatformError::MapFailed)?;

            // SAFETY: fmap returned a mapping of `len` bytes.
            unsafe { core::ptr::write_bytes(virt as *mut u8, 0, len) };
            Ok(Self {
                virt: virt as *mut u8,
                phys,
                len,
                scheme_fd,
            })
        }
    }

    impl DmaRegion for RedoxDma {
        fn cpu_ptr(&self) -> *mut u8 {
            self.virt
        }
        fn phys_addr(&self) -> u64 {
            // The device-visible physical base, read back from the allocation
            // path — this is what gets programmed into virtqueue descriptors.
            self.phys as u64
        }
        fn len(&self) -> usize {
            self.len
        }
    }

    impl Drop for RedoxDma {
        fn drop(&mut self) {
            let _ = unsafe { funmap(self.virt as usize, self.len) };
            let _ = close(self.scheme_fd);
        }
    }

    /// A Redox platform binding holding the device's IRQ handle.
    pub struct RedoxPlatform {
        irq_fd: usize,
    }

    impl RedoxPlatform {
        /// Open the `irq` scheme handle for the device's interrupt line.
        pub fn new(irq: u8) -> Result<Self, PlatformError> {
            let irq_fd = openat(ROOT_FD, format!("irq:{irq}"), O_RDWR | O_CLOEXEC, 0)
                .map_err(|_| PlatformError::Unsupported)?;
            Ok(Self { irq_fd })
        }
    }

    impl Platform for RedoxPlatform {
        type Dma = RedoxDma;

        fn alloc_dma(&mut self, len: usize) -> Result<RedoxDma, PlatformError> {
            RedoxDma::new(len)
        }

        unsafe fn map_mmio(&mut self, phys: u64, len: usize) -> Result<Bank, PlatformError> {
            let scheme_fd = openat(ROOT_FD, "memory:", O_RDWR | O_CLOEXEC, 0)
                .map_err(|_| PlatformError::MapFailed)?;
            let map = Map {
                offset: phys as usize,
                size: len,
                flags: map_flags(),
                address: 0,
            };
            let virt = unsafe { fmap(scheme_fd, &map) }.map_err(|_| PlatformError::MapFailed)?;
            // SAFETY: fmap returned a mapping covering the window.
            Ok(unsafe { Bank::new(virt as *mut u8) })
        }

        fn wait_irq(&mut self) -> Result<(), PlatformError> {
            // Reading the irq handle blocks until the interrupt fires.
            let mut count = [0u8; 8];
            let n = read(self.irq_fd, &mut count).map_err(|_| PlatformError::Closed)?;
            if n == 0 {
                return Err(PlatformError::Closed);
            }
            Ok(())
        }

        fn ack_irq(&mut self) -> Result<(), PlatformError> {
            // Writing the count back acknowledges and re-arms the interrupt.
            let count = [0u8; 8];
            write(self.irq_fd, &count).map_err(|_| PlatformError::Closed)?;
            Ok(())
        }
    }

    impl Drop for RedoxPlatform {
        fn drop(&mut self) {
            let _ = close(self.irq_fd);
        }
    }
}

#[cfg(not(target_os = "redox"))]
mod imp {
    use super::*;

    /// Off-Redox stub so the crate and daemon logic compile on any host.
    pub struct RedoxDma;
    impl DmaRegion for RedoxDma {
        fn cpu_ptr(&self) -> *mut u8 {
            core::ptr::null_mut()
        }
        fn phys_addr(&self) -> u64 {
            0
        }
        fn len(&self) -> usize {
            0
        }
    }

    /// Off-Redox stub platform.
    pub struct RedoxPlatform;
    impl RedoxPlatform {
        pub fn new(_irq: u8) -> Result<Self, PlatformError> {
            Err(PlatformError::Unsupported)
        }
    }
    impl Platform for RedoxPlatform {
        type Dma = RedoxDma;
        fn alloc_dma(&mut self, _len: usize) -> Result<RedoxDma, PlatformError> {
            Err(PlatformError::Unsupported)
        }
        unsafe fn map_mmio(&mut self, _phys: u64, _len: usize) -> Result<Bank, PlatformError> {
            Err(PlatformError::Unsupported)
        }
        fn wait_irq(&mut self) -> Result<(), PlatformError> {
            Err(PlatformError::Closed)
        }
        fn ack_irq(&mut self) -> Result<(), PlatformError> {
            Ok(())
        }
    }
}

pub use imp::{RedoxDma, RedoxPlatform};
