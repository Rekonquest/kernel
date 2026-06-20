//! Platform seam + interrupt-driven service loop.
//!
//! A virtio driver needs four things from the OS, and nothing else: DMA-coherent
//! memory, a mapped MMIO window, and a way to wait for and acknowledge the
//! device interrupt. [`Platform`] is that seam. Implement it once per OS — on
//! Redox via `physalloc`/`physmap` and the `irq` scheme — and the driver loop in
//! [`run`] is identical everywhere, including under a mock in tests.
//!
//! This is the kit's discipline applied to the OS itself: the driver is the
//! orchestrator, the platform is dumb mechanism behind a narrow contract, and
//! the cost (a blocking IRQ wait, an explicit ack) is never hidden.

use crate::{dma::DmaRegion, mmio::Bank, virtio::VirtioMmio};

/// What a platform binding can fail with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformError {
    /// DMA allocation failed.
    OutOfMemory,
    /// Mapping an MMIO window failed.
    MapFailed,
    /// The interrupt source closed; the driver should stop cleanly.
    Closed,
    /// The platform does not provide this operation.
    Unsupported,
}

/// The OS services a virtio driver depends on.
pub trait Platform {
    /// The DMA region type this platform hands out.
    type Dma: DmaRegion;

    /// Allocate a DMA-coherent region of at least `len` bytes.
    fn alloc_dma(&mut self, len: usize) -> Result<Self::Dma, PlatformError>;

    /// Map a device MMIO window into a register [`Bank`].
    ///
    /// # Safety
    /// `phys`/`len` must name a real, exclusively-owned device MMIO window.
    unsafe fn map_mmio(&mut self, phys: u64, len: usize) -> Result<Bank, PlatformError>;

    /// Block until the device raises an interrupt. Returns
    /// [`PlatformError::Closed`] when the driver should stop.
    fn wait_irq(&mut self) -> Result<(), PlatformError>;

    /// Acknowledge the interrupt at the OS level (after the device-level ack).
    fn ack_irq(&mut self) -> Result<(), PlatformError>;
}

/// Run the interrupt-driven service loop until the interrupt source closes.
///
/// Each iteration: block on the device IRQ, snapshot+clear the device's
/// interrupt status, run `service` (which polls the queues and does device
/// work), then acknowledge at both the device and OS level. Returns `Ok` on a
/// clean shutdown ([`PlatformError::Closed`] from [`Platform::wait_irq`]).
pub fn run<P: Platform>(
    platform: &mut P,
    mmio: &VirtioMmio,
    mut service: impl FnMut(),
) -> Result<(), PlatformError> {
    loop {
        match platform.wait_irq() {
            Ok(()) => {}
            Err(PlatformError::Closed) => return Ok(()),
            Err(other) => return Err(other),
        }
        // Acknowledge the device interrupt *before* draining the queues. If we
        // acked after, a completion that lands between the service closure's
        // last poll and the ack would have its freshly-set interrupt-status bit
        // cleared by this stale ack, leaving a used-ring entry pending with no
        // armed IRQ. Acking first means any post-drain completion re-raises the
        // interrupt and wakes the next iteration.
        let causes = mmio.interrupt_status();
        mmio.ack_interrupt(causes);
        service();
        platform.ack_irq()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::transport::{regs, MAGIC_VALUE};

    /// A DMA region that is never actually used in these tests.
    struct NoDma;
    impl DmaRegion for NoDma {
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

    /// Delivers `budget` interrupts, then signals shutdown. Counts acks.
    struct MockPlatform {
        budget: u32,
        irqs_delivered: u32,
        acks: u32,
    }
    impl Platform for MockPlatform {
        type Dma = NoDma;
        fn alloc_dma(&mut self, _len: usize) -> Result<NoDma, PlatformError> {
            Err(PlatformError::Unsupported)
        }
        unsafe fn map_mmio(&mut self, _phys: u64, _len: usize) -> Result<Bank, PlatformError> {
            Err(PlatformError::Unsupported)
        }
        fn wait_irq(&mut self) -> Result<(), PlatformError> {
            if self.irqs_delivered < self.budget {
                self.irqs_delivered += 1;
                Ok(())
            } else {
                Err(PlatformError::Closed)
            }
        }
        fn ack_irq(&mut self) -> Result<(), PlatformError> {
            self.acks += 1;
            Ok(())
        }
    }

    fn window() -> Vec<u32> {
        let mut regs = vec![0u32; 0x80];
        regs[regs::MAGIC / 4] = MAGIC_VALUE;
        regs[regs::VERSION / 4] = 2;
        regs[regs::DEVICE_ID / 4] = 1;
        regs
    }

    #[test]
    fn service_loop_runs_once_per_irq_and_acks() {
        let regs = window();
        let bank = unsafe { Bank::new(regs.as_ptr() as *mut u8) };
        let mmio = unsafe { VirtioMmio::new(bank).unwrap() };

        let mut plat = MockPlatform {
            budget: 3,
            irqs_delivered: 0,
            acks: 0,
        };
        let mut serviced = 0u32;
        let result = run(&mut plat, &mmio, || serviced += 1);

        assert_eq!(result, Ok(()));
        assert_eq!(serviced, 3, "service ran once per delivered interrupt");
        assert_eq!(plat.acks, 3, "every interrupt was acknowledged");
    }

    #[test]
    fn loop_exits_cleanly_when_no_interrupts() {
        let regs = window();
        let bank = unsafe { Bank::new(regs.as_ptr() as *mut u8) };
        let mmio = unsafe { VirtioMmio::new(bank).unwrap() };
        let mut plat = MockPlatform {
            budget: 0,
            irqs_delivered: 0,
            acks: 0,
        };
        assert_eq!(run(&mut plat, &mmio, || panic!("must not service")), Ok(()));
    }
}
