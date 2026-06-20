//! `virtio-netd` — a Redox userspace virtio-net driver daemon.
//!
//! Maps the device, negotiates features, sets up the RX/TX virtqueues, pre-posts
//! a pool of receive buffers, arms the device, then runs the kit's
//! interrupt-driven service loop draining TX completions and received frames.
//! The device logic is entirely `driver-primitives`; this file is the Redox
//! wiring. Exposing a `network:` scheme to the rest of the OS is the remaining
//! glue (see `src/scheme.rs`).
//!
//! Build for Redox: `cargo build --release --target x86_64-unknown-redox`.

use driver_primitives::{
    dma::DmaRegion,
    runtime::{self, Platform, PlatformError},
    virtio::{net, RxPool, VirtQueue, VirtioMmio, VirtioNet},
};
use redox_virtio_drivers::{DeviceLocation, RedoxPlatform};

const QSIZE: usize = 256;
const RX_POOL: usize = 16;
const RX_BUF_LEN: usize = 2048;

fn main() {
    let location = DeviceLocation::from_env();
    match run(location) {
        Ok(()) => {}
        Err(error) => eprintln!("virtio-netd: stopped ({error:?})"),
    }
}

fn run(location: DeviceLocation) -> Result<(), PlatformError> {
    let mut platform = RedoxPlatform::new(location.irq)?;

    // Map the device registers and bring the device up.
    let bank = unsafe { platform.map_mmio(location.mmio_phys, location.mmio_len) }?;
    let mmio = unsafe { VirtioMmio::new(bank) }.map_err(|_| PlatformError::Unsupported)?;
    let agreed = net::negotiate(mmio.device_features()).map_err(|_| PlatformError::Unsupported)?;
    mmio.accept_features(agreed)
        .map_err(|_| PlatformError::Unsupported)?;

    // Receive data buffers, allocated first so they outlive the queues that
    // reference them.
    let rx_bufs: Vec<_> = (0..RX_POOL)
        .map(|_| platform.alloc_dma(RX_BUF_LEN))
        .collect::<Result<Vec<_>, _>>()?;

    // Queue 0 = receive, queue 1 = transmit.
    let rx_region = platform.alloc_dma(VirtQueue::<QSIZE>::required_bytes())?;
    let tx_region = platform.alloc_dma(VirtQueue::<QSIZE>::required_bytes())?;
    let rx =
        unsafe { VirtQueue::<QSIZE>::new(&rx_region) }.map_err(|_| PlatformError::Unsupported)?;
    let tx =
        unsafe { VirtQueue::<QSIZE>::new(&tx_region) }.map_err(|_| PlatformError::Unsupported)?;
    mmio.setup_queue(0, &rx)
        .map_err(|_| PlatformError::Unsupported)?;
    mmio.setup_queue(1, &tx)
        .map_err(|_| PlatformError::Unsupported)?;
    mmio.set_driver_ok();

    let mut nic = VirtioNet::new(rx, tx, agreed);

    // Pre-post every receive buffer so the device can deliver into them, then
    // tell it the RX ring is ready.
    let descs: [(u64, u32); RX_POOL] =
        core::array::from_fn(|i| (rx_bufs[i].phys_addr(), RX_BUF_LEN as u32));
    let mut rx_pool: RxPool<RX_POOL> = RxPool::new(descs);
    rx_pool
        .post_all(&mut nic.rx)
        .map_err(|_| PlatformError::Unsupported)?;
    mmio.notify(0);

    // Interrupt-driven service loop: free completed transmits, take delivered
    // frames, and immediately re-post their buffers to keep the RX ring full.
    runtime::run(&mut platform, &mmio, || {
        while nic.poll_tx().is_some() {}
        let mut received_any = false;
        while let Some((index, _len)) = rx_pool.poll(&mut nic.rx) {
            // A full driver hands the frame (rx_bufs[index], _len) to the
            // network: scheme here before reposting.
            let _ = rx_pool.repost(&mut nic.rx, index);
            received_any = true;
        }
        if received_any {
            mmio.notify(0);
        }
    })
}
