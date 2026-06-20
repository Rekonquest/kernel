//! Driver-shape integration test: a mock NIC TX path built entirely from the
//! kit's public API. This is the "ready for hardware" proof — the control path
//! (submit → dispatch → complete → reap), DMA descriptors, and an MMIO doorbell
//! compose into a driver shape, exercised end to end on the host. Swapping the
//! mock device for a real virtio transport (real `DmaRegion`, real doorbell
//! register) is the only remaining step to drive metal.

use driver_primitives::dma::{Descriptor, DmaRegion};
use driver_primitives::mmio::Reg;
use driver_primitives::Reactor;

/// A heap-backed stand-in for a DMA-capable frame buffer. On Redox this role is
/// played by `redox_syscall::Dma`, whose physical address and CPU mapping
/// satisfy the same `DmaRegion` contract.
struct Frame {
    bytes: Vec<u8>,
    phys: u64,
}

impl DmaRegion for Frame {
    fn cpu_ptr(&self) -> *mut u8 {
        self.bytes.as_ptr() as *mut u8
    }
    fn phys_addr(&self) -> u64 {
        self.phys
    }
    fn len(&self) -> usize {
        self.bytes.len()
    }
}

#[test]
fn nic_tx_path_composes_from_the_kit() {
    // A doorbell register the driver kicks to notify the device. Here it is a
    // plain cell; on hardware it is a register inside a mapped MMIO window.
    let mut doorbell_cell: u32 = 0;
    let doorbell = unsafe { Reg::new(&mut doorbell_cell as *mut u32) };

    // The NIC's TX engine: descriptors in, bytes-sent out; 16 in flight, 1 flow.
    let mut nic: Reactor<Descriptor, u32, 16, 1> = Reactor::new();
    nic.register_client(0, 1);

    // Frames of varied sizes to transmit.
    let frames: Vec<Frame> = (0..4)
        .map(|i| Frame {
            bytes: vec![0xAB; 64 + i * 16],
            phys: 0x1000 + (i as u64) * 0x200,
        })
        .collect();

    // Driver: post a TX descriptor per frame (device-readable) and ring the
    // doorbell to notify the device.
    let mut ids = Vec::new();
    for frame in &frames {
        let desc = Descriptor::for_region(frame, 0); // flags 0 => device reads it (TX)
        ids.push(nic.submit(0, desc).unwrap());
        doorbell.modify(|kicks| kicks + 1);
    }
    assert_eq!(doorbell.read(), 4, "driver rang the doorbell once per frame");

    // Device: drain the queue, "transmit" each descriptor, and report bytes sent.
    while let Some((ticket, desc)) = nic.dispatch() {
        nic.complete(ticket, desc.len);
    }

    // Driver: reap completions. Each frame's byte count returns, matched to its
    // submission by sequence, in order.
    let mut completions = Vec::new();
    while let Some(cqe) = nic.reap() {
        completions.push((cqe.seq, cqe.result));
    }

    assert_eq!(completions.len(), 4);
    for (i, frame) in frames.iter().enumerate() {
        assert_eq!(completions[i].0, ids[i].0, "completion seq matches submission");
        assert_eq!(completions[i].1 as usize, frame.len(), "bytes sent == frame length");
    }
    assert!(nic.is_idle(), "no work left outstanding");
    assert_eq!(nic.lost_completions(), 0);
}
