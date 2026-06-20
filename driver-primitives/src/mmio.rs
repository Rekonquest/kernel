//! Volatile register access — the one place the kit touches real hardware.
//!
//! Device registers are not normal memory: reads and writes have side effects
//! and must not be reordered or elided by the compiler. [`Reg`] is a thin,
//! typed volatile accessor over a raw pointer, and [`Bank`] mints registers at
//! byte offsets within a mapped MMIO window. This is the kit's only `unsafe`
//! surface, kept small and explicit on purpose.
//!
//! Volatile guarantees ordering *with respect to the device*, not against other
//! CPUs — a register shared between cores still needs a lock. The kit stays
//! portable: the orchestrator maps the MMIO window (on Redox via the `memory`
//! scheme / physmap) and hands the base pointer in.

use core::ptr;

/// A typed volatile handle to a single hardware register.
#[derive(Clone, Copy, Debug)]
pub struct Reg<T: Copy> {
    ptr: *mut T,
}

impl<T: Copy> Reg<T> {
    /// Wrap a raw register pointer.
    ///
    /// # Safety
    /// `ptr` must be non-null, aligned for `T`, and remain a valid MMIO or
    /// memory location for as long as this `Reg` (and its copies) are used.
    /// The caller is responsible for any cross-CPU synchronization.
    pub const unsafe fn new(ptr: *mut T) -> Self {
        Self { ptr }
    }

    /// Volatile read.
    pub fn read(self) -> T {
        // SAFETY: validity guaranteed by the `new` contract.
        unsafe { ptr::read_volatile(self.ptr) }
    }

    /// Volatile write.
    pub fn write(self, value: T) {
        // SAFETY: validity guaranteed by the `new` contract.
        unsafe { ptr::write_volatile(self.ptr, value) }
    }

    /// Read-modify-write. Note this is not atomic against the device or other
    /// CPUs; use only where that is safe (single owner).
    pub fn modify(self, f: impl FnOnce(T) -> T) {
        let value = self.read();
        self.write(f(value));
    }

    /// The underlying pointer.
    pub const fn as_ptr(self) -> *mut T {
        self.ptr
    }
}

/// A base address for a register bank; mints [`Reg`]s at byte offsets.
#[derive(Clone, Copy, Debug)]
pub struct Bank {
    base: *mut u8,
}

impl Bank {
    /// Wrap the base of a mapped MMIO window.
    ///
    /// # Safety
    /// `base` must point at a valid mapping covering every offset later passed
    /// to [`reg`](Bank::reg), for the lifetime of this `Bank`.
    pub const unsafe fn new(base: *mut u8) -> Self {
        Self { base }
    }

    /// A register of type `T` at `offset` bytes from the base.
    ///
    /// # Safety
    /// `offset` must lie within the mapped window and be aligned for `T`.
    pub unsafe fn reg<T: Copy>(self, offset: usize) -> Reg<T> {
        // SAFETY: caller guarantees offset is in-range and aligned.
        unsafe { Reg::new(self.base.add(offset).cast::<T>()) }
    }

    /// The base pointer.
    pub const fn base(self) -> *mut u8 {
        self.base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_modify_round_trips() {
        let mut cell: u32 = 0;
        let reg = unsafe { Reg::new(&mut cell as *mut u32) };
        reg.write(0xDEAD_0000);
        assert_eq!(reg.read(), 0xDEAD_0000);
        reg.modify(|v| v | 0xBEEF);
        assert_eq!(reg.read(), 0xDEAD_BEEF);
    }

    #[test]
    fn bank_addresses_registers_by_offset() {
        let mut regs: [u32; 4] = [0; 4];
        let bank = unsafe { Bank::new(regs.as_mut_ptr().cast::<u8>()) };
        let r0 = unsafe { bank.reg::<u32>(0) };
        let r2 = unsafe { bank.reg::<u32>(8) };
        r0.write(11);
        r2.write(22);
        assert_eq!(r0.read(), 11);
        assert_eq!(r2.read(), 22);
    }
}
