//! `virtio-netd` — a Redox userspace virtio-net driver daemon.
//!
//! Maps the device, negotiates features, sets up the RX/TX virtqueues, arms the
//! device, then runs the kit's interrupt-driven service loop. The device logic
//! is entirely `driver-primitives`; this file is just the Redox wiring.
//!
//! Build for Redox: `cargo build --release --target x86_64-unknown-redox`.

use driver_primitives::runtime::{self, Platform, PlatformError};
use driver_primitives::virtio::{net, VirtQueue, VirtioMmio, VirtioNet};
use redox_virtio_drivers::{DeviceLocation, RedoxPlatform};

const QSIZE: usize = 256;

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
    mmio.accept_features(agreed).map_err(|_| PlatformError::Unsupported)?;

    // Queue 0 = receive, queue 1 = transmit. Their backing regions outlive the
    // queues (declared first, dropped last).
    let rx_region = platform.alloc_dma(VirtQueue::<QSIZE>::required_bytes())?;
    let tx_region = platform.alloc_dma(VirtQueue::<QSIZE>::required_bytes())?;
    let rx = unsafe { VirtQueue::<QSIZE>::new(&rx_region) }.map_err(|_| PlatformError::Unsupported)?;
    let tx = unsafe { VirtQueue::<QSIZE>::new(&tx_region) }.map_err(|_| PlatformError::Unsupported)?;
    mmio.setup_queue(0, &rx).map_err(|_| PlatformError::Unsupported)?;
    mmio.setup_queue(1, &tx).map_err(|_| PlatformError::Unsupported)?;
    mmio.set_driver_ok();

    let mut nic = VirtioNet::new(rx, tx, agreed);

    // A full driver pre-posts RX buffers and registers a `network:` scheme here.
    // The skeleton just drains completions on every interrupt.
    runtime::run(&mut platform, &mmio, || {
        while nic.poll_tx().is_some() {}
        while nic.poll_rx().is_some() {}
    })
}
