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
        flag::{PhysmapFlags, O_CLOEXEC, O_RDWR},
        open, physalloc, physfree, physmap, physunmap, read, write,
    };

    /// DMA-coherent memory: a contiguous physical allocation mapped for the CPU.
    pub struct RedoxDma {
        virt: *mut u8,
        phys: usize,
        len: usize,
    }

    impl RedoxDma {
        pub fn new(len: usize) -> Result<Self, PlatformError> {
            let phys = physalloc(len).map_err(|_| PlatformError::OutOfMemory)?;
            let virt = unsafe { physmap(phys, len, PhysmapFlags::PHYSMAP_WRITE) }
                .map_err(|_| PlatformError::MapFailed)?;
            // SAFETY: physmap returned a mapping of `len` bytes.
            unsafe { core::ptr::write_bytes(virt as *mut u8, 0, len) };
            Ok(Self {
                virt: virt as *mut u8,
                phys,
                len,
            })
        }
    }

    impl DmaRegion for RedoxDma {
        fn cpu_ptr(&self) -> *mut u8 {
            self.virt
        }
        fn phys_addr(&self) -> u64 {
            self.phys as u64
        }
        fn len(&self) -> usize {
            self.len
        }
    }

    impl Drop for RedoxDma {
        fn drop(&mut self) {
            let _ = unsafe { physunmap(self.virt as usize) };
            let _ = physfree(self.phys, self.len);
        }
    }

    /// A Redox platform binding holding the device's IRQ handle.
    pub struct RedoxPlatform {
        irq_fd: usize,
    }

    impl RedoxPlatform {
        /// Open the `irq` scheme handle for the device's interrupt line.
        pub fn new(irq: u8) -> Result<Self, PlatformError> {
            let path = format!("irq:{irq}");
            let irq_fd = open(&path, O_RDWR | O_CLOEXEC).map_err(|_| PlatformError::Unsupported)?;
            Ok(Self { irq_fd })
        }
    }

    impl Platform for RedoxPlatform {
        type Dma = RedoxDma;

        fn alloc_dma(&mut self, len: usize) -> Result<RedoxDma, PlatformError> {
            RedoxDma::new(len)
        }

        unsafe fn map_mmio(&mut self, phys: u64, len: usize) -> Result<Bank, PlatformError> {
            let virt = unsafe {
                physmap(
                    phys as usize,
                    len,
                    PhysmapFlags::PHYSMAP_WRITE | PhysmapFlags::PHYSMAP_NO_CACHE,
                )
            }
            .map_err(|_| PlatformError::MapFailed)?;
            // SAFETY: physmap returned a mapping covering the window.
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
