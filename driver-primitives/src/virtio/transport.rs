//! virtio-mmio transport — the device bring-up handshake over real registers.
//!
//! This is the last piece between the kit and a live virtio device: discover the
//! device, negotiate features, hand it the queue addresses, and arm it. It talks
//! to the device through [`mmio::Bank`]/[`mmio::Reg`] (real volatile register
//! access), so the same code drives QEMU's virtio-mmio device once the register
//! window is mapped (on Redox, via the `memory` scheme).
//!
//! Bring-up sequence (virtio 1.0, MMIO transport):
//! 1. verify MagicValue / Version / DeviceID;
//! 2. reset, then set ACKNOWLEDGE | DRIVER;
//! 3. read DeviceFeatures, write DriverFeatures, set FEATURES_OK and confirm;
//! 4. for each queue: program desc/avail/used addresses and mark it ready;
//! 5. set DRIVER_OK.
//!
//! [`mmio::Bank`]: crate::mmio::Bank
//! [`mmio::Reg`]: crate::mmio::Reg

use crate::feature::Features;
use crate::mmio::Bank;

use super::queue::VirtQueue;

/// virtio-mmio register offsets (bytes from the window base).
pub mod regs {
    pub const MAGIC: usize = 0x000;
    pub const VERSION: usize = 0x004;
    pub const DEVICE_ID: usize = 0x008;
    pub const VENDOR_ID: usize = 0x00c;
    pub const DEVICE_FEATURES: usize = 0x010;
    pub const DEVICE_FEATURES_SEL: usize = 0x014;
    pub const DRIVER_FEATURES: usize = 0x020;
    pub const DRIVER_FEATURES_SEL: usize = 0x024;
    pub const QUEUE_SEL: usize = 0x030;
    pub const QUEUE_NUM_MAX: usize = 0x034;
    pub const QUEUE_NUM: usize = 0x038;
    pub const QUEUE_READY: usize = 0x044;
    pub const QUEUE_NOTIFY: usize = 0x050;
    pub const INTERRUPT_STATUS: usize = 0x060;
    pub const INTERRUPT_ACK: usize = 0x064;
    pub const STATUS: usize = 0x070;
    pub const QUEUE_DESC_LOW: usize = 0x080;
    pub const QUEUE_DESC_HIGH: usize = 0x084;
    pub const QUEUE_DRIVER_LOW: usize = 0x090;
    pub const QUEUE_DRIVER_HIGH: usize = 0x094;
    pub const QUEUE_DEVICE_LOW: usize = 0x0a0;
    pub const QUEUE_DEVICE_HIGH: usize = 0x0a4;
    pub const CONFIG: usize = 0x100;
}

/// Device status bits.
pub mod status {
    pub const ACKNOWLEDGE: u32 = 1;
    pub const DRIVER: u32 = 2;
    pub const DRIVER_OK: u32 = 4;
    pub const FEATURES_OK: u32 = 8;
    pub const DEVICE_NEEDS_RESET: u32 = 64;
    pub const FAILED: u32 = 128;
}

/// "virt" — the value the MagicValue register must read.
pub const MAGIC_VALUE: u32 = 0x7472_6976;
/// The modern (non-legacy) transport version.
pub const VERSION_MODERN: u32 = 2;

/// Bring-up failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportError {
    /// MagicValue did not read "virt".
    BadMagic,
    /// Version was not the modern (2) transport.
    UnsupportedVersion,
    /// DeviceID was 0 (no device present).
    NoDevice,
    /// The device rejected the negotiated feature set.
    FeaturesNotAccepted,
    /// The selected queue is unavailable (QueueNumMax == 0).
    QueueUnavailable,
    /// The requested queue size exceeds the device maximum.
    QueueTooLarge,
}

/// A virtio-mmio device behind a mapped register window.
#[derive(Clone, Copy)]
pub struct VirtioMmio {
    bank: Bank,
}

impl VirtioMmio {
    /// Discover and reset a virtio-mmio device, advancing it to
    /// ACKNOWLEDGE | DRIVER.
    ///
    /// # Safety
    /// `bank` must point at a mapped virtio-mmio register window (at least
    /// through the config area) that stays valid for the device's lifetime.
    pub unsafe fn new(bank: Bank) -> Result<Self, TransportError> {
        let dev = Self { bank };
        if dev.read(regs::MAGIC) != MAGIC_VALUE {
            return Err(TransportError::BadMagic);
        }
        if dev.read(regs::VERSION) != VERSION_MODERN {
            return Err(TransportError::UnsupportedVersion);
        }
        if dev.read(regs::DEVICE_ID) == 0 {
            return Err(TransportError::NoDevice);
        }
        dev.write(regs::STATUS, 0); // reset
        dev.add_status(status::ACKNOWLEDGE);
        dev.add_status(status::DRIVER);
        Ok(dev)
    }

    /// The virtio device type (1 = net, 16 = gpu, ...).
    pub fn device_id(&self) -> u32 {
        self.read(regs::DEVICE_ID)
    }

    /// The current device status register.
    pub fn status(&self) -> u32 {
        self.read(regs::STATUS)
    }

    /// The 64-bit feature set the device offers.
    pub fn device_features(&self) -> Features {
        self.write(regs::DEVICE_FEATURES_SEL, 0);
        let low = self.read(regs::DEVICE_FEATURES) as u64;
        self.write(regs::DEVICE_FEATURES_SEL, 1);
        let high = self.read(regs::DEVICE_FEATURES) as u64;
        Features::from_bits((high << 32) | low)
    }

    /// Write the driver's chosen features and confirm the device accepted them.
    pub fn accept_features(&self, features: Features) -> Result<(), TransportError> {
        let bits = features.bits();
        self.write(regs::DRIVER_FEATURES_SEL, 0);
        self.write(regs::DRIVER_FEATURES, bits as u32);
        self.write(regs::DRIVER_FEATURES_SEL, 1);
        self.write(regs::DRIVER_FEATURES, (bits >> 32) as u32);
        self.add_status(status::FEATURES_OK);
        if self.read(regs::STATUS) & status::FEATURES_OK != 0 {
            Ok(())
        } else {
            Err(TransportError::FeaturesNotAccepted)
        }
    }

    /// The device's maximum size for queue `index`.
    pub fn queue_num_max(&self, index: u16) -> u32 {
        self.write(regs::QUEUE_SEL, index as u32);
        self.read(regs::QUEUE_NUM_MAX)
    }

    /// Program queue `index` with the addresses of `vq` and mark it ready.
    pub fn setup_queue<const Q: usize>(
        &self,
        index: u16,
        vq: &VirtQueue<Q>,
    ) -> Result<(), TransportError> {
        self.write(regs::QUEUE_SEL, index as u32);
        let max = self.read(regs::QUEUE_NUM_MAX);
        if max == 0 {
            return Err(TransportError::QueueUnavailable);
        }
        if Q as u32 > max {
            return Err(TransportError::QueueTooLarge);
        }
        self.write(regs::QUEUE_NUM, Q as u32);

        let (desc, avail, used) = (vq.desc_addr(), vq.avail_addr(), vq.used_addr());
        self.write(regs::QUEUE_DESC_LOW, desc as u32);
        self.write(regs::QUEUE_DESC_HIGH, (desc >> 32) as u32);
        self.write(regs::QUEUE_DRIVER_LOW, avail as u32);
        self.write(regs::QUEUE_DRIVER_HIGH, (avail >> 32) as u32);
        self.write(regs::QUEUE_DEVICE_LOW, used as u32);
        self.write(regs::QUEUE_DEVICE_HIGH, (used >> 32) as u32);
        self.write(regs::QUEUE_READY, 1);
        Ok(())
    }

    /// Mark the driver live; the device may now run.
    pub fn set_driver_ok(&self) {
        self.add_status(status::DRIVER_OK);
    }

    /// Notify the device that queue `index` has new buffers.
    pub fn notify(&self, index: u16) {
        self.write(regs::QUEUE_NOTIFY, index as u32);
    }

    /// Pending interrupt causes.
    pub fn interrupt_status(&self) -> u32 {
        self.read(regs::INTERRUPT_STATUS)
    }

    /// Acknowledge interrupt causes.
    pub fn ack_interrupt(&self, causes: u32) {
        self.write(regs::INTERRUPT_ACK, causes);
    }

    /// Read a 32-bit word from device-specific config space.
    pub fn config_read32(&self, offset: usize) -> u32 {
        self.read(regs::CONFIG + offset)
    }

    // --- register helpers (the bank covers the whole window per `new`) -------

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: offset is a documented in-window register; `new` requires the
        // bank to cover the window.
        unsafe { self.bank.reg::<u32>(offset) }.read()
    }

    fn write(&self, offset: usize, value: u32) {
        // SAFETY: as above.
        unsafe { self.bank.reg::<u32>(offset) }.write(value)
    }

    fn add_status(&self, bits: u32) {
        let current = self.read(regs::STATUS);
        self.write(regs::STATUS, current | bits);
    }
}
