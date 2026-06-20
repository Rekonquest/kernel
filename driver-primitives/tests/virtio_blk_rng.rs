//! Storage (virtio-blk) and entropy (virtio-rng) end to end against mock
//! devices — two more device classes on the same kit.

use core::ptr;
use core::sync::atomic::{fence, Ordering};

use driver_primitives::dma::DmaRegion;
use driver_primitives::virtio::{blk, VirtioBlk, VirtioRng, VirtQueue};

const Q: usize = 64;

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

/// Walks a descriptor chain from `head`, returning each (addr, len, writable).
unsafe fn chain(base: *mut u8, head: u16) -> Vec<(u64, u32, bool)> {
    let mut out = Vec::new();
    let mut idx = head;
    loop {
        let d = unsafe { base.add(idx as usize * 16) };
        let addr = u64::from_le(unsafe { ptr::read_volatile(d as *const u64) });
        let len = u32::from_le(unsafe { ptr::read_volatile(d.add(8) as *const u32) });
        let flags = u16::from_le(unsafe { ptr::read_volatile(d.add(12) as *const u16) });
        let next = u16::from_le(unsafe { ptr::read_volatile(d.add(14) as *const u16) });
        out.push((addr, len, flags & 2 != 0));
        if flags & 1 == 0 {
            break;
        }
        idx = next;
    }
    out
}

struct Mock {
    base: *mut u8,
    avail_off: usize,
    used_off: usize,
    last_avail: u16,
    used_idx: u16,
}
impl Mock {
    fn new(region: &Dma) -> Self {
        Mock {
            base: region.cpu_ptr(),
            avail_off: VirtQueue::<Q>::AVAIL_OFFSET,
            used_off: VirtQueue::<Q>::USED_OFFSET,
            last_avail: 0,
            used_idx: 0,
        }
    }
    unsafe fn next_head(&mut self) -> Option<u16> {
        let avail = unsafe { self.base.add(self.avail_off) };
        let avail_idx = u16::from_le(unsafe { ptr::read_volatile(avail.add(2) as *const u16) });
        if self.last_avail == avail_idx {
            return None;
        }
        fence(Ordering::Acquire);
        let slot = (self.last_avail % Q as u16) as usize;
        let head = u16::from_le(unsafe { ptr::read_volatile(avail.add(4 + slot * 2) as *const u16) });
        self.last_avail = self.last_avail.wrapping_add(1);
        Some(head)
    }
    unsafe fn complete(&mut self, head: u16, len: u32) {
        let used = unsafe { self.base.add(self.used_off) };
        let uslot = (self.used_idx % Q as u16) as usize;
        let elem = unsafe { used.add(4 + uslot * 8) };
        unsafe {
            ptr::write_volatile(elem as *mut u32, (head as u32).to_le());
            ptr::write_volatile(elem.add(4) as *mut u32, len.to_le());
        }
        self.used_idx = self.used_idx.wrapping_add(1);
        fence(Ordering::Release);
        unsafe { ptr::write_volatile(used.add(2) as *mut u16, self.used_idx.to_le()) };
    }
}

#[test]
fn blk_read_fills_data_and_reports_status() {
    let region = Dma::new(VirtQueue::<Q>::required_bytes());
    let mut dev = VirtioBlk::new(unsafe { VirtQueue::<Q>::new(&region).unwrap() });
    let mut mock = Mock::new(&region);

    let hdr = Dma::new(blk::HDR_LEN);
    let data = Dma::new(blk::SECTOR_SIZE);
    let st = Dma::new(1);

    let head = dev.read(&hdr, &data, &st, 42).unwrap();

    // Device side: confirm the request header, fill the data, write OK status.
    unsafe {
        let h = mock.next_head().unwrap();
        assert_eq!(h, head);
        let segs = chain(mock.base, h);
        assert_eq!(segs.len(), 3);
        // Header (readable): type IN, sector 42.
        assert_eq!(u32::from_le(ptr::read_volatile(segs[0].0 as *const u32)), blk::req::IN);
        assert_eq!(u64::from_le(ptr::read_volatile((segs[0].0 + 8) as *const u64)), 42);
        assert!(!segs[0].2); // header is device-readable
        assert!(segs[1].2); // data is device-writable for a read
        // Fill the data buffer and set status OK.
        for i in 0..segs[1].1 as usize {
            ptr::write_volatile((segs[1].0 as *mut u8).add(i), 0xEE);
        }
        ptr::write_volatile(segs[2].0 as *mut u8, blk::status::OK);
        mock.complete(h, segs[1].1 + 1);
    }

    assert!(dev.poll().is_some());
    assert_eq!(VirtioBlk::<Q>::status_of(&st), blk::status::OK);
    let got = unsafe { core::slice::from_raw_parts(data.cpu_ptr(), blk::SECTOR_SIZE) };
    assert!(got.iter().all(|&b| b == 0xEE));
    assert_eq!(dev.queue.num_free(), Q as u16, "all three descriptors freed");
}

#[test]
fn rng_fills_the_requested_buffer() {
    let region = Dma::new(VirtQueue::<Q>::required_bytes());
    let mut dev = VirtioRng::new(unsafe { VirtQueue::<Q>::new(&region).unwrap() });
    let mut mock = Mock::new(&region);

    let buf = Dma::new(32);
    let head = dev.request(&buf).unwrap();

    unsafe {
        let h = mock.next_head().unwrap();
        assert_eq!(h, head);
        let segs = chain(mock.base, h);
        assert_eq!(segs.len(), 1);
        assert!(segs[0].2); // single device-writable buffer
        for i in 0..segs[0].1 as usize {
            ptr::write_volatile((segs[0].0 as *mut u8).add(i), (i as u8).wrapping_mul(7));
        }
        mock.complete(h, segs[0].1);
    }

    let used = dev.poll().unwrap();
    assert_eq!(used.len, 32);
    let got = unsafe { core::slice::from_raw_parts(buf.cpu_ptr(), 32) };
    assert_eq!(got[1], 7);
    assert_eq!(got[2], 14);
}
