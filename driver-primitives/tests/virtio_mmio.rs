//! virtio-mmio bring-up test: drive the device init handshake over a mock
//! register file and verify the driver discovers, negotiates, programs the
//! queue addresses, and arms the device exactly as the spec requires.
//!
//! The "device" is a flat register window the test pre-loads (read-only regs)
//! and inspects (write-only regs) — the same window a real device exposes.

use driver_primitives::dma::DmaRegion;
use driver_primitives::mmio::Bank;
use driver_primitives::virtio::transport::{regs, status, MAGIC_VALUE};
use driver_primitives::virtio::{TransportError, VirtQueue, VirtioMmio};

const Q: usize = 64;

/// 16-byte-aligned DMA region for the virtqueue.
struct DmaBuf {
    mem: Vec<u128>,
    len: usize,
}
impl DmaBuf {
    fn new(bytes: usize) -> Self {
        let units = bytes.div_ceil(16).max(1);
        DmaBuf { mem: vec![0u128; units], len: units * 16 }
    }
}
impl DmaRegion for DmaBuf {
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

/// A flat virtio-mmio register window (offset/4 indexes the word).
fn reg_window() -> (Vec<u32>, Bank) {
    let regs = vec![0u32; 0x80]; // covers offsets 0x000..0x200
    let bank = unsafe { Bank::new(regs.as_ptr() as *mut u8) };
    (regs, bank)
}

fn rd(bank: Bank, off: usize) -> u32 {
    unsafe { bank.reg::<u32>(off) }.read()
}
fn wr(bank: Bank, off: usize, v: u32) {
    unsafe { bank.reg::<u32>(off) }.write(v)
}

#[test]
fn rejects_a_bad_magic() {
    let (_regs, bank) = reg_window();
    // MagicValue left at 0.
    assert_eq!(
        unsafe { VirtioMmio::new(bank) }.err(),
        Some(TransportError::BadMagic)
    );
}

#[test]
fn full_bring_up_sequence() {
    let (_regs, bank) = reg_window();
    // Device presents itself: magic, modern version, net device, a feature, a
    // queue max size.
    wr(bank, regs::MAGIC, MAGIC_VALUE);
    wr(bank, regs::VERSION, 2);
    wr(bank, regs::DEVICE_ID, 1); // virtio-net
    wr(bank, regs::DEVICE_FEATURES, 0x20); // low-word features
    wr(bank, regs::QUEUE_NUM_MAX, 256);

    let dev = unsafe { VirtioMmio::new(bank) }.unwrap();
    assert_eq!(dev.device_id(), 1);
    // After discovery the driver has acknowledged and claimed the device.
    assert_eq!(
        dev.status() & (status::ACKNOWLEDGE | status::DRIVER),
        status::ACKNOWLEDGE | status::DRIVER
    );

    // Feature low word is read back correctly.
    assert_eq!(dev.device_features().bits() as u32, 0x20);

    // Negotiate (mock accepts whatever; FEATURES_OK sticks in the flat window).
    dev.accept_features(driver_primitives::Features::from_bits(0x20))
        .unwrap();
    assert_ne!(dev.status() & status::FEATURES_OK, 0);

    // Program a queue and confirm the addresses landed in the registers.
    let region = DmaBuf::new(VirtQueue::<Q>::required_bytes());
    let vq = unsafe { VirtQueue::<Q>::new(&region).unwrap() };
    dev.setup_queue(0, &vq).unwrap();

    assert_eq!(rd(bank, regs::QUEUE_NUM), Q as u32);
    assert_eq!(rd(bank, regs::QUEUE_READY), 1);
    let desc = ((rd(bank, regs::QUEUE_DESC_HIGH) as u64) << 32) | rd(bank, regs::QUEUE_DESC_LOW) as u64;
    let avail = ((rd(bank, regs::QUEUE_DRIVER_HIGH) as u64) << 32) | rd(bank, regs::QUEUE_DRIVER_LOW) as u64;
    let used = ((rd(bank, regs::QUEUE_DEVICE_HIGH) as u64) << 32) | rd(bank, regs::QUEUE_DEVICE_LOW) as u64;
    assert_eq!(desc, vq.desc_addr());
    assert_eq!(avail, vq.avail_addr());
    assert_eq!(used, vq.used_addr());

    // Arm the device and kick the queue.
    dev.set_driver_ok();
    assert_ne!(dev.status() & status::DRIVER_OK, 0);
    dev.notify(0);
    assert_eq!(rd(bank, regs::QUEUE_NOTIFY), 0);

    // Interrupt ack path.
    wr(bank, regs::INTERRUPT_STATUS, 0x1);
    assert_eq!(dev.interrupt_status(), 0x1);
    dev.ack_interrupt(0x1);
    assert_eq!(rd(bank, regs::INTERRUPT_ACK), 0x1);
}

#[test]
fn queue_too_large_is_rejected() {
    let (_regs, bank) = reg_window();
    wr(bank, regs::MAGIC, MAGIC_VALUE);
    wr(bank, regs::VERSION, 2);
    wr(bank, regs::DEVICE_ID, 1);
    wr(bank, regs::QUEUE_NUM_MAX, 16); // device max smaller than our Q=64

    let dev = unsafe { VirtioMmio::new(bank) }.unwrap();
    let region = DmaBuf::new(VirtQueue::<Q>::required_bytes());
    let vq = unsafe { VirtQueue::<Q>::new(&region).unwrap() };
    assert_eq!(dev.setup_queue(0, &vq), Err(TransportError::QueueTooLarge));
}
