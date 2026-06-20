//! RX path test: pre-post receive buffers, have a mock device deliver a frame
//! into one, reap it through the pool, and re-post it — the receive side of a
//! real virtio-net driver, exercised on the host.

use core::ptr;
use core::sync::atomic::{fence, Ordering};

use driver_primitives::dma::DmaRegion;
use driver_primitives::virtio::{RxPool, VirtQueue};

const Q: usize = 64;
const POOL: usize = 8;

struct Dma {
    mem: Vec<u128>,
    len: usize,
}
impl Dma {
    fn new(bytes: usize) -> Self {
        let units = bytes.div_ceil(16).max(1);
        Dma { mem: vec![0u128; units], len: units * 16 }
    }
}
impl DmaRegion for Dma {
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

/// Mock RX device: takes a posted (device-writable) buffer, writes `pattern`
/// bytes into it, and completes with that length.
struct MockRx {
    base: *mut u8,
    avail_off: usize,
    used_off: usize,
    last_avail: u16,
    used_idx: u16,
}
impl MockRx {
    unsafe fn deliver(&mut self, nbytes: u32, pattern: u8) -> bool {
        let avail = unsafe { self.base.add(self.avail_off) };
        let avail_idx = u16::from_le(unsafe { ptr::read_volatile(avail.add(2) as *const u16) });
        if self.last_avail == avail_idx {
            return false;
        }
        fence(Ordering::Acquire);
        let slot = (self.last_avail % Q as u16) as usize;
        let head = u16::from_le(unsafe { ptr::read_volatile(avail.add(4 + slot * 2) as *const u16) });
        self.last_avail = self.last_avail.wrapping_add(1);

        // The head descriptor points at a writable buffer; fill it.
        let d = unsafe { self.base.add(head as usize * 16) };
        let addr = u64::from_le(unsafe { ptr::read_volatile(d as *const u64) });
        let buf = addr as *mut u8;
        for i in 0..nbytes as usize {
            unsafe { ptr::write_volatile(buf.add(i), pattern) };
        }

        // Complete.
        let used = unsafe { self.base.add(self.used_off) };
        let uslot = (self.used_idx % Q as u16) as usize;
        let elem = unsafe { used.add(4 + uslot * 8) };
        unsafe {
            ptr::write_volatile(elem as *mut u32, (head as u32).to_le());
            ptr::write_volatile(elem.add(4) as *mut u32, nbytes.to_le());
        }
        self.used_idx = self.used_idx.wrapping_add(1);
        fence(Ordering::Release);
        unsafe { ptr::write_volatile(used.add(2) as *mut u16, self.used_idx.to_le()) };
        true
    }
}

#[test]
fn receive_buffers_are_posted_filled_and_reposted() {
    let region = Dma::new(VirtQueue::<Q>::required_bytes());
    let mut rx = unsafe { VirtQueue::<Q>::new(&region).unwrap() };
    let mut dev = MockRx {
        base: region.cpu_ptr(),
        avail_off: VirtQueue::<Q>::AVAIL_OFFSET,
        used_off: VirtQueue::<Q>::USED_OFFSET,
        last_avail: 0,
        used_idx: 0,
    };

    // Allocate POOL receive buffers and pre-post them all.
    let bufs: Vec<Dma> = (0..POOL).map(|_| Dma::new(2048)).collect();
    let descs: [(u64, u32); POOL] = core::array::from_fn(|i| (bufs[i].phys_addr(), 2048));
    let mut pool: RxPool<POOL> = RxPool::new(descs);
    pool.post_all(&mut rx).unwrap();
    assert_eq!(pool.posted(), POOL, "all buffers posted to the device");

    // Device delivers a 100-byte frame.
    assert!(unsafe { dev.deliver(100, 0xCD) });

    // Driver reaps it.
    let (index, len) = pool.poll(&mut rx).unwrap();
    assert_eq!(len, 100);
    assert_eq!(pool.posted(), POOL - 1, "the filled buffer is no longer posted");

    // The bytes really landed in that pool buffer.
    let received = unsafe { core::slice::from_raw_parts(bufs[index].cpu_ptr(), 100) };
    assert!(received.iter().all(|&b| b == 0xCD));

    // Consume and re-post; the ring is full again.
    pool.repost(&mut rx, index).unwrap();
    assert_eq!(pool.posted(), POOL);

    // A second frame can be delivered after reposting.
    assert!(unsafe { dev.deliver(64, 0xAB) });
    let (_index2, len2) = pool.poll(&mut rx).unwrap();
    assert_eq!(len2, 64);
}
