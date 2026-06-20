//! Input (virtio-input) and audio (virtio-snd) end to end against mock devices.

use core::ptr;
use core::sync::atomic::{fence, Ordering};

use driver_primitives::dma::DmaRegion;
use driver_primitives::virtio::input::{self, ev};
use driver_primitives::virtio::{snd, VirtioInput, VirtioSnd, VirtQueue};

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

/// Generic per-queue mock: pull the next available head and post completions.
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
    unsafe fn desc(&self, idx: u16) -> (u64, u32, u16, u16) {
        let d = unsafe { self.base.add(idx as usize * 16) };
        (
            u64::from_le(unsafe { ptr::read_volatile(d as *const u64) }),
            u32::from_le(unsafe { ptr::read_volatile(d.add(8) as *const u32) }),
            u16::from_le(unsafe { ptr::read_volatile(d.add(12) as *const u16) }),
            u16::from_le(unsafe { ptr::read_volatile(d.add(14) as *const u16) }),
        )
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
fn input_events_are_delivered_and_parsed() {
    const POOL: usize = 8;
    let region = Dma::new(VirtQueue::<Q>::required_bytes());
    let eventq = unsafe { VirtQueue::<Q>::new(&region).unwrap() };
    let bufs: Vec<Dma> = (0..POOL).map(|_| Dma::new(input::EVENT_LEN)).collect();
    let descs: [(u64, u32); POOL] =
        core::array::from_fn(|i| (bufs[i].phys_addr(), input::EVENT_LEN as u32));
    let mut dev: VirtioInput<Q, POOL> = VirtioInput::new(eventq, descs);
    let mut mock = Mock::new(&region);

    dev.start().unwrap();

    // Device delivers a key-press event (EV_KEY, code 30 = 'A', value 1).
    unsafe {
        let head = mock.next_head().unwrap();
        let (addr, _len, _flags, _next) = mock.desc(head);
        let p = addr as *mut u8;
        ptr::write_volatile(p as *mut u16, ev::KEY.to_le());
        ptr::write_volatile(p.add(2) as *mut u16, 30u16.to_le());
        ptr::write_volatile(p.add(4) as *mut u32, 1u32.to_le());
        mock.complete(head, input::EVENT_LEN as u32);
    }

    let (index, len) = dev.poll().unwrap();
    assert_eq!(len, input::EVENT_LEN as u32);
    let event = input::parse_event(&bufs[index]);
    assert_eq!(event.kind, ev::KEY);
    assert_eq!(event.code, 30);
    assert_eq!(event.value, 1);

    dev.repost(index).unwrap();
}

#[test]
fn audio_set_params_start_and_play() {
    let cregion = Dma::new(VirtQueue::<Q>::required_bytes());
    let tregion = Dma::new(VirtQueue::<Q>::required_bytes());
    let control = unsafe { VirtQueue::<Q>::new(&cregion).unwrap() };
    let tx = unsafe { VirtQueue::<Q>::new(&tregion).unwrap() };
    let mut dev: VirtioSnd<Q, Q> = VirtioSnd::new(control, tx);
    let mut cmock = Mock::new(&cregion);
    let mut tmock = Mock::new(&tregion);

    let cmd = Dma::new(snd::SET_PARAMS_LEN);
    let resp = Dma::new(8);

    // set_params: confirm the device sees the right command code, ack OK.
    dev.set_params(&cmd, &resp, 0, 8192, 2048, 2, snd::format::S16, snd::rate::R48000)
        .unwrap();
    unsafe {
        let head = cmock.next_head().unwrap();
        let (addr, _l, _f, _n) = cmock.desc(head);
        assert_eq!(u32::from_le(ptr::read_volatile(addr as *const u32)), snd::code::PCM_SET_PARAMS);
        // Write an OK response into the writable descriptor (the next one).
        let (_a, _l2, _f2, next) = cmock.desc(head);
        let (resp_addr, _rl, _rf, _rn) = cmock.desc(next);
        ptr::write_volatile(resp_addr as *mut u32, snd::resp::OK.to_le());
        cmock.complete(head, 4);
    }
    assert!(dev.poll_control().is_some());
    assert_eq!(snd::VirtioSnd::<Q, Q>::response_status(&resp), snd::resp::OK);

    // start.
    dev.start(&cmd, &resp, 0).unwrap();
    unsafe {
        let head = cmock.next_head().unwrap();
        let (addr, _l, _f, next) = cmock.desc(head);
        assert_eq!(u32::from_le(ptr::read_volatile(addr as *const u32)), snd::code::PCM_START);
        let (resp_addr, _rl, _rf, _rn) = cmock.desc(next);
        ptr::write_volatile(resp_addr as *mut u32, snd::resp::OK.to_le());
        cmock.complete(head, 4);
    }
    assert!(dev.poll_control().is_some());

    // play: submit a PCM buffer; device reads it and writes an OK status.
    let xfer = Dma::new(snd::XFER_LEN);
    let audio = Dma::new(2048);
    let status = Dma::new(snd::PCM_STATUS_LEN);
    dev.play(&xfer, &audio, &status, 0).unwrap();
    unsafe {
        let head = tmock.next_head().unwrap();
        // chain: xfer (ro), audio (ro), status (wo)
        let (_xa, _xl, _xf, n1) = tmock.desc(head);
        let (_aa, _al, _af, n2) = tmock.desc(n1);
        let (status_addr, _sl, _sf, _sn) = tmock.desc(n2);
        ptr::write_volatile(status_addr as *mut u32, snd::resp::OK.to_le());
        ptr::write_volatile((status_addr + 4) as *mut u32, 0u32.to_le()); // latency
        tmock.complete(head, snd::PCM_STATUS_LEN as u32);
    }
    assert!(dev.poll_tx().is_some());
    assert_eq!(
        u32::from_le(unsafe { ptr::read_volatile(status.cpu_ptr() as *const u32) }),
        snd::resp::OK
    );
    assert_eq!(dev.tx.num_free(), Q as u16, "playback descriptors freed");
}
