//! End-to-end multi-screen path on (mock) virtio-gpu hardware:
//!
//!   GET_DISPLAY_INFO  ->  discover scanouts
//!         |
//!         v
//!   Display::commit   ->  configure every monitor in ONE atomic modeset
//!         |
//!         v
//!   create/attach/set_scanout/transfer/flush  ->  present a frame
//!
//! The driver code is real (the kit's VirtQueue + virtio-gpu command
//! serialization + the Display orchestrator). Only the device is mocked: it
//! parses the control queue and answers like QEMU's virtio-gpu would.

use core::{
    ptr,
    sync::atomic::{fence, Ordering},
};

use driver_primitives::{
    display::{Change, Display, Mode},
    dma::DmaRegion,
    txn::Transaction,
    virtio::{
        gpu::{self, ty, Rect},
        VirtQueue, VirtioGpu,
    },
};

const Q: usize = 64;

struct Dma {
    mem: Vec<u128>,
    len: usize,
}
impl Dma {
    fn new(bytes: usize) -> Self {
        let units = bytes.div_ceil(16).max(1);
        Dma {
            mem: vec![0u128; units],
            len: units * 16,
        }
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

/// Mock virtio-gpu device: reads the control queue's [cmd, resp] chains and
/// answers each command, reporting two enabled monitors for display-info.
struct MockGpu {
    base: *mut u8,
    avail_off: usize,
    used_off: usize,
    last_avail: u16,
    used_idx: u16,
    last_cmd_type: u32,
}

impl MockGpu {
    unsafe fn service_one(&mut self) -> bool {
        let avail = unsafe { self.base.add(self.avail_off) };
        let avail_idx = u16::from_le(unsafe { ptr::read_volatile(avail.add(2) as *const u16) });
        if self.last_avail == avail_idx {
            return false;
        }
        fence(Ordering::Acquire);
        let slot = (self.last_avail % Q as u16) as usize;
        let head =
            u16::from_le(unsafe { ptr::read_volatile(avail.add(4 + slot * 2) as *const u16) });
        self.last_avail = self.last_avail.wrapping_add(1);

        // Descriptor chain: head = command (readable), next = response (writable).
        let d0 = unsafe { self.base.add(head as usize * 16) };
        let cmd_addr = u64::from_le(unsafe { ptr::read_volatile(d0 as *const u64) });
        let next = u16::from_le(unsafe { ptr::read_volatile(d0.add(14) as *const u16) });
        let d1 = unsafe { self.base.add(next as usize * 16) };
        let resp_addr = u64::from_le(unsafe { ptr::read_volatile(d1 as *const u64) });

        let cmd_type = u32::from_le(unsafe { ptr::read_volatile(cmd_addr as *const u32) });
        self.last_cmd_type = cmd_type;
        let resp = resp_addr as *mut u8;

        let resp_len = if cmd_type == ty::GET_DISPLAY_INFO {
            // Response header.
            unsafe { ptr::write_volatile(resp as *mut u32, ty::RESP_OK_DISPLAY_INFO.to_le()) };
            // Zero the rest of the header (flags/fence/ctx/pad) then the pmodes.
            for i in 1..(gpu::RESP_DISPLAY_INFO_LEN / 4) {
                unsafe { ptr::write_volatile((resp as *mut u32).add(i), 0u32.to_le()) };
            }
            // Two enabled scanouts: 1920x1080 and 1280x720.
            let modes = [(1920u32, 1080u32), (1280, 720)];
            for (i, (w, h)) in modes.into_iter().enumerate() {
                let off = gpu::CTRL_HDR_LEN + i * gpu::DISPLAY_ONE_LEN;
                let p = unsafe { resp.add(off) } as *mut u32;
                unsafe {
                    ptr::write_volatile(p, 0u32.to_le()); // rect.x
                    ptr::write_volatile(p.add(1), 0u32.to_le()); // rect.y
                    ptr::write_volatile(p.add(2), w.to_le()); // rect.width
                    ptr::write_volatile(p.add(3), h.to_le()); // rect.height
                    ptr::write_volatile(p.add(4), 1u32.to_le()); // enabled
                    ptr::write_volatile(p.add(5), 0u32.to_le()); // flags
                }
            }
            gpu::RESP_DISPLAY_INFO_LEN
        } else {
            unsafe { ptr::write_volatile(resp as *mut u32, ty::RESP_OK_NODATA.to_le()) };
            gpu::RESP_NODATA_LEN
        };

        // Post the completion.
        let used = unsafe { self.base.add(self.used_off) };
        let uslot = (self.used_idx % Q as u16) as usize;
        let elem = unsafe { used.add(4 + uslot * 8) };
        unsafe {
            ptr::write_volatile(elem as *mut u32, (head as u32).to_le());
            ptr::write_volatile(elem.add(4) as *mut u32, (resp_len as u32).to_le());
        }
        self.used_idx = self.used_idx.wrapping_add(1);
        fence(Ordering::Release);
        unsafe { ptr::write_volatile(used.add(2) as *mut u16, self.used_idx.to_le()) };
        true
    }
}

#[test]
fn discover_monitors_configure_atomically_and_present() {
    let region = Dma::new(VirtQueue::<Q>::required_bytes());
    let mut gpu = VirtioGpu::new(unsafe { VirtQueue::<Q>::new(&region).unwrap() });
    let mut dev = MockGpu {
        base: region.cpu_ptr(),
        avail_off: VirtQueue::<Q>::AVAIL_OFFSET,
        used_off: VirtQueue::<Q>::USED_OFFSET,
        last_avail: 0,
        used_idx: 0,
        last_cmd_type: 0,
    };

    // 1. Discover scanouts.
    let cmd = Dma::new(gpu::CMD_MAX_LEN);
    let resp = Dma::new(gpu::RESP_DISPLAY_INFO_LEN);
    let head = gpu.get_display_info(&cmd, &resp).unwrap();
    assert!(unsafe { dev.service_one() });
    assert_eq!(gpu.poll().unwrap().head, head);
    assert_eq!(gpu::response_type(&resp), ty::RESP_OK_DISPLAY_INFO);

    let (scanouts, count) = gpu::parse_display_info(&resp);
    assert_eq!(count, 2, "device reported two monitors");
    assert_eq!(scanouts[0].rect, Rect::new(0, 0, 1920, 1080));
    assert_eq!(scanouts[1].rect, Rect::new(0, 0, 1280, 720));

    // 2. Feed the discovered monitors into the display orchestrator and bring
    //    them all up in ONE atomic modeset.
    let mut display: Display<{ gpu::MAX_SCANOUTS }> = Display::new();
    let mut txn: Transaction<Change, 32> = Transaction::new();
    for (i, s) in scanouts.iter().enumerate() {
        if s.enabled {
            let mode = Mode::new(s.rect.width, s.rect.height, 60000);
            display.connect(i, mode);
            txn.stage(Change::SetMode { output: i, mode }).unwrap();
            txn.stage(Change::SetFb {
                output: i,
                fb: 100 + i as u32,
            })
            .unwrap();
        }
    }
    assert!(
        display.commit(txn).is_ok(),
        "all monitors configured atomically"
    );
    assert_eq!(
        display.output(0).unwrap().mode(),
        Some(Mode::new(1920, 1080, 60000))
    );
    assert_eq!(
        display.output(1).unwrap().mode(),
        Some(Mode::new(1280, 720, 60000))
    );

    // 3. Present a frame on scanout 0: create resource, attach backing, bind to
    //    the scanout, transfer pixels, flush.
    let fb = Dma::new(1920 * 1080 * 4);
    let rect0 = Rect::new(0, 0, 1920, 1080);
    let steps: &[(&str, u32)] = &[
        ("create", ty::RESOURCE_CREATE_2D),
        ("attach", ty::RESOURCE_ATTACH_BACKING),
        ("scanout", ty::SET_SCANOUT),
        ("transfer", ty::TRANSFER_TO_HOST_2D),
        ("flush", ty::RESOURCE_FLUSH),
    ];

    gpu.create_resource_2d(&cmd, &resp, 1, gpu::format::B8G8R8A8_UNORM, 1920, 1080)
        .unwrap();
    assert!(unsafe { dev.service_one() });
    assert_eq!(dev.last_cmd_type, steps[0].1);
    assert!(gpu.poll().is_some());

    gpu.attach_backing(&cmd, &resp, 1, fb.phys_addr(), fb.len() as u32)
        .unwrap();
    assert!(unsafe { dev.service_one() });
    assert_eq!(dev.last_cmd_type, steps[1].1);
    assert!(gpu.poll().is_some());

    gpu.set_scanout(&cmd, &resp, 0, 1, rect0).unwrap();
    assert!(unsafe { dev.service_one() });
    assert_eq!(dev.last_cmd_type, steps[2].1);
    assert!(gpu.poll().is_some());

    gpu.transfer_to_host_2d(&cmd, &resp, 1, rect0, 0).unwrap();
    assert!(unsafe { dev.service_one() });
    assert_eq!(dev.last_cmd_type, steps[3].1);
    assert!(gpu.poll().is_some());

    gpu.resource_flush(&cmd, &resp, 1, rect0).unwrap();
    assert!(unsafe { dev.service_one() });
    assert_eq!(dev.last_cmd_type, steps[4].1);
    assert!(gpu.poll().is_some());

    // The control queue is balanced again after the whole present sequence.
    assert_eq!(gpu.control.num_free(), Q as u16);
}

#[test]
fn present_issues_transfer_then_flush() {
    let region = Dma::new(VirtQueue::<Q>::required_bytes());
    let mut gpu = VirtioGpu::new(unsafe { VirtQueue::<Q>::new(&region).unwrap() });
    let mut dev = MockGpu {
        base: region.cpu_ptr(),
        avail_off: VirtQueue::<Q>::AVAIL_OFFSET,
        used_off: VirtQueue::<Q>::USED_OFFSET,
        last_avail: 0,
        used_idx: 0,
        last_cmd_type: 0,
    };

    // Two command/response pairs so transfer and flush pipeline.
    let (tc, tr) = (Dma::new(gpu::CMD_MAX_LEN), Dma::new(gpu::RESP_NODATA_LEN));
    let (fc, fr) = (Dma::new(gpu::CMD_MAX_LEN), Dma::new(gpu::RESP_NODATA_LEN));
    let rect = Rect::new(0, 0, 1920, 1080);

    gpu.present(&tc, &tr, &fc, &fr, 1, rect, 0).unwrap();

    // The device sees the transfer first...
    assert!(unsafe { dev.service_one() });
    assert_eq!(dev.last_cmd_type, ty::TRANSFER_TO_HOST_2D);
    assert!(gpu.poll().is_some());
    // ...then the flush.
    assert!(unsafe { dev.service_one() });
    assert_eq!(dev.last_cmd_type, ty::RESOURCE_FLUSH);
    assert!(gpu.poll().is_some());

    assert_eq!(gpu.control.num_free(), Q as u16, "both commands freed");
}
