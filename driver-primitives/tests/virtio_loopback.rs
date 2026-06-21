//! Faithful virtio split-virtqueue test: drive the real virtio memory protocol
//! against a mock device that plays the hardware side.
//!
//! The driver (`VirtQueue`) and the device (`MockDevice`) share only the DMA
//! region and the documented layout — exactly as real hardware would. The
//! device reads the available ring, walks the descriptor table, and posts to the
//! used ring; the driver reaps. If this passes, the same `add_buf`/`poll_used`
//! code drives a real virtio device once the addresses are programmed into it.

use core::{
    ptr,
    sync::atomic::{fence, Ordering},
};

use driver_primitives::{
    dma::DmaRegion,
    virtio::{Segment, VirtQueue},
};

const Q: usize = 256;

/// A 16-byte-aligned, heap-backed DMA region. On the host, physical == virtual
/// (identity map), which is all the mock device needs to follow descriptors.
struct DmaBuf {
    mem: Vec<u128>,
    len: usize,
}

impl DmaBuf {
    fn new(bytes: usize) -> Self {
        let units = bytes.div_ceil(16);
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
        self.mem.as_ptr() as u64 // identity map for the host test
    }
    fn len(&self) -> usize {
        self.len
    }
}

/// The hardware side of the protocol: consumes the available ring and posts to
/// the used ring, reading buffer bytes through descriptor (physical==virtual)
/// addresses.
struct MockDevice {
    base: *mut u8,
    avail_off: usize,
    used_off: usize,
    last_avail: u16,
    used_idx: u16,
}

impl MockDevice {
    /// Service one available buffer, optionally summing its readable bytes.
    /// Returns the bytes "consumed" reported back in the used ring, or `None`
    /// if there was nothing to do.
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

        // Walk the descriptor chain, summing readable bytes to prove the device
        // really followed the addresses we published.
        let mut total: u32 = 0;
        let mut idx = head;
        loop {
            let d = unsafe { self.base.add(idx as usize * 16) };
            let addr = u64::from_le(unsafe { ptr::read_volatile(d as *const u64) });
            let len = u32::from_le(unsafe { ptr::read_volatile(d.add(8) as *const u32) });
            let flags = u16::from_le(unsafe { ptr::read_volatile(d.add(12) as *const u16) });
            let next = u16::from_le(unsafe { ptr::read_volatile(d.add(14) as *const u16) });

            // F_WRITE (2) means device-writable; otherwise it is readable input.
            if flags & 2 == 0 {
                let bytes = addr as *const u8;
                for i in 0..len as usize {
                    total = total.wrapping_add(unsafe { *bytes.add(i) } as u32);
                }
            }
            if flags & 1 == 0 {
                break; // no F_NEXT
            }
            idx = next;
        }

        // Post the completion into the used ring.
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
fn driver_and_device_round_trip_a_buffer() {
    let region = DmaBuf::new(VirtQueue::<Q>::required_bytes());
    let mut vq = unsafe { VirtQueue::<Q>::new(&region).unwrap() };
    let mut dev = MockDevice {
        base: region.cpu_ptr(),
        avail_off: VirtQueue::<Q>::AVAIL_OFFSET,
        used_off: VirtQueue::<Q>::USED_OFFSET,
        last_avail: 0,
        used_idx: 0,
    };

    // A "packet" buffer filled with known bytes; identity-mapped so the device
    // can follow the descriptor address.
    let mut packet = DmaBuf::new(4);
    unsafe {
        let p = packet.cpu_ptr();
        ptr::write_volatile(p, 1);
        ptr::write_volatile(p.add(1), 2);
        ptr::write_volatile(p.add(2), 3);
        ptr::write_volatile(p.add(3), 4);
    }
    let _ = &mut packet;

    let head = vq
        .add_buf(&[Segment {
            addr: packet.phys_addr(),
            len: 4,
            device_writable: false,
        }])
        .unwrap();
    assert_eq!(vq.num_free(), Q as u16 - 1);

    // Device services it and sums the bytes (1+2+3+4 = 10).
    let consumed = unsafe { dev.service_one() }.unwrap();
    assert_eq!(
        consumed, 10,
        "device followed the descriptor and read the buffer"
    );

    // Driver reaps and the descriptor returns to the free list.
    let used = vq.poll_used().unwrap();
    assert_eq!(used.head, head);
    assert_eq!(used.len, 10);
    assert_eq!(vq.num_free(), Q as u16, "chain fully freed");
    assert!(vq.poll_used().is_none());
}

#[test]
fn multi_descriptor_chain_is_built_and_freed() {
    let region = DmaBuf::new(VirtQueue::<Q>::required_bytes());
    let mut vq = unsafe { VirtQueue::<Q>::new(&region).unwrap() };
    let mut dev = MockDevice {
        base: region.cpu_ptr(),
        avail_off: VirtQueue::<Q>::AVAIL_OFFSET,
        used_off: VirtQueue::<Q>::USED_OFFSET,
        last_avail: 0,
        used_idx: 0,
    };

    // virtio-net shape: a small readable header + a readable data buffer.
    let mut hdr = DmaBuf::new(2);
    let mut data = DmaBuf::new(3);
    unsafe {
        ptr::write_volatile(hdr.cpu_ptr(), 10);
        ptr::write_volatile(hdr.cpu_ptr().add(1), 20);
        ptr::write_volatile(data.cpu_ptr(), 5);
        ptr::write_volatile(data.cpu_ptr().add(1), 6);
        ptr::write_volatile(data.cpu_ptr().add(2), 7);
    }
    let _ = (&mut hdr, &mut data);

    vq.add_buf(&[
        Segment {
            addr: hdr.phys_addr(),
            len: 2,
            device_writable: false,
        },
        Segment {
            addr: data.phys_addr(),
            len: 3,
            device_writable: false,
        },
    ])
    .unwrap();
    assert_eq!(vq.num_free(), Q as u16 - 2, "two descriptors taken");

    let consumed = unsafe { dev.service_one() }.unwrap();
    assert_eq!(consumed, 10 + 20 + 5 + 6 + 7); // device walked the whole chain

    let used = vq.poll_used().unwrap();
    assert_eq!(used.len, 48);
    assert_eq!(vq.num_free(), Q as u16, "both descriptors freed");
}

#[test]
fn many_buffers_keep_the_free_list_balanced() {
    let region = DmaBuf::new(VirtQueue::<Q>::required_bytes());
    let mut vq = unsafe { VirtQueue::<Q>::new(&region).unwrap() };
    let mut dev = MockDevice {
        base: region.cpu_ptr(),
        avail_off: VirtQueue::<Q>::AVAIL_OFFSET,
        used_off: VirtQueue::<Q>::USED_OFFSET,
        last_avail: 0,
        used_idx: 0,
    };
    let buf = DmaBuf::new(8);

    // Push far more buffers than the queue holds at once, draining as we go,
    // to exercise index wraparound and free-list balance.
    for _ in 0..1000 {
        vq.add_buf(&[Segment {
            addr: buf.phys_addr(),
            len: 8,
            device_writable: false,
        }])
        .unwrap();
        assert!(unsafe { dev.service_one() }.is_some());
        assert!(vq.poll_used().is_some());
    }
    assert_eq!(
        vq.num_free(),
        Q as u16,
        "no descriptors leaked over 1000 cycles"
    );
}
