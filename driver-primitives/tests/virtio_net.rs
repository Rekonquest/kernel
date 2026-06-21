//! virtio-net orchestrator end to end: transmit a frame through the real TX
//! virtqueue to a mock device, which reads the [net header, frame] chain and
//! reports the bytes it saw. Proves the orchestrator + queue + DMA seam compose
//! into a working network TX path; only the real transport (PCI/virtio-mmio
//! discovery, IRQ binding) remains for actual hardware.

use core::{
    ptr,
    sync::atomic::{fence, Ordering},
};

use driver_primitives::{
    dma::DmaRegion,
    virtio::{net::VirtioNetHdr, Segment, VirtQueue, VirtioNet},
};

const Q: usize = 64;

struct DmaBuf {
    mem: Vec<u128>,
    len: usize,
}

impl DmaBuf {
    fn new(bytes: usize) -> Self {
        let units = bytes.div_ceil(16).max(1);
        DmaBuf {
            mem: vec![0u128; units],
            len: units * 16,
        }
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

/// Services the TX queue: reads the available chain, sums all readable bytes,
/// posts to the used ring.
struct MockDevice {
    base: *mut u8,
    avail_off: usize,
    used_off: usize,
    last_avail: u16,
    used_idx: u16,
}

impl MockDevice {
    unsafe fn service_one(&mut self) -> Option<u32> {
        let avail = unsafe { self.base.add(self.avail_off) };
        let avail_idx = u16::from_le(unsafe { ptr::read_volatile(avail.add(2) as *const u16) });
        if self.last_avail == avail_idx {
            return None;
        }
        fence(Ordering::Acquire);
        let slot = (self.last_avail % Q as u16) as usize;
        let head =
            u16::from_le(unsafe { ptr::read_volatile(avail.add(4 + slot * 2) as *const u16) });
        self.last_avail = self.last_avail.wrapping_add(1);

        let mut total: u32 = 0;
        let mut idx = head;
        loop {
            let d = unsafe { self.base.add(idx as usize * 16) };
            let addr = u64::from_le(unsafe { ptr::read_volatile(d as *const u64) });
            let len = u32::from_le(unsafe { ptr::read_volatile(d.add(8) as *const u32) });
            let flags = u16::from_le(unsafe { ptr::read_volatile(d.add(12) as *const u16) });
            let next = u16::from_le(unsafe { ptr::read_volatile(d.add(14) as *const u16) });
            if flags & 2 == 0 {
                let bytes = addr as *const u8;
                for i in 0..len as usize {
                    total = total.wrapping_add(unsafe { *bytes.add(i) } as u32);
                }
            }
            if flags & 1 == 0 {
                break;
            }
            idx = next;
        }

        let used = unsafe { self.base.add(self.used_off) };
        let uslot = (self.used_idx % Q as u16) as usize;
        let elem = unsafe { used.add(4 + uslot * 8) };
        unsafe {
            ptr::write_volatile(elem as *mut u32, (head as u32).to_le());
            ptr::write_volatile(elem.add(4) as *mut u32, total.to_le());
        }
        self.used_idx = self.used_idx.wrapping_add(1);
        fence(Ordering::Release);
        unsafe { ptr::write_volatile(used.add(2) as *mut u16, self.used_idx.to_le()) };
        Some(total)
    }
}

#[test]
fn transmit_a_frame_end_to_end() {
    let rx_region = DmaBuf::new(VirtQueue::<Q>::required_bytes());
    let tx_region = DmaBuf::new(VirtQueue::<Q>::required_bytes());
    let rx = unsafe { VirtQueue::<Q>::new(&rx_region).unwrap() };
    let tx = unsafe { VirtQueue::<Q>::new(&tx_region).unwrap() };

    // Pretend the device offered VERSION_1 so negotiation succeeds.
    let agreed =
        driver_primitives::virtio::net::negotiate(driver_primitives::Features::bit(32)).unwrap();
    let mut nic = VirtioNet::new(rx, tx, agreed);

    let mut dev = MockDevice {
        base: tx_region.cpu_ptr(),
        avail_off: VirtQueue::<Q>::AVAIL_OFFSET,
        used_off: VirtQueue::<Q>::USED_OFFSET,
        last_avail: 0,
        used_idx: 0,
    };

    // A header buffer (orchestrator fills it) and a frame with known bytes.
    let hdr = DmaBuf::new(VirtioNetHdr::LEN);
    let mut frame = DmaBuf::new(4);
    unsafe {
        let p = frame.cpu_ptr();
        ptr::write_volatile(p, 100);
        ptr::write_volatile(p.add(1), 50);
        ptr::write_volatile(p.add(2), 25);
        ptr::write_volatile(p.add(3), 5);
    }
    let _ = &mut frame;

    let head = nic
        .transmit(
            &hdr,
            Segment {
                addr: frame.phys_addr(),
                len: 4,
                device_writable: false,
            },
        )
        .unwrap();
    // Two descriptors used: header + frame.
    assert_eq!(nic.tx.num_free(), Q as u16 - 2);

    // Device reads the chain. Header is all zeros, so the byte sum is the frame.
    let seen = unsafe { dev.service_one() }.unwrap();
    assert_eq!(seen, 100 + 50 + 25 + 5);

    // Driver reaps the completed transmit; descriptors return to the free list.
    let used = nic.poll_tx().unwrap();
    assert_eq!(used.head, head);
    assert_eq!(
        nic.tx.num_free(),
        Q as u16,
        "header + frame descriptors freed"
    );
}
