//! `virtio-rngd` — a Redox userspace virtio-rng (entropy) driver daemon.
//!
//! Brings the device up, requests entropy into a buffer, and on each completion
//! re-requests to keep entropy flowing. A full driver feeds the system RNG /
//! exposes a `rand:` scheme.
//!
//! Build for Redox: `cargo build --release --target x86_64-unknown-redox`.

use driver_primitives::runtime::{self, Platform, PlatformError};
use driver_primitives::virtio::{VirtQueue, VirtioMmio, VirtioRng};
use driver_primitives::Features;
use redox_virtio_drivers::{DeviceLocation, RedoxPlatform};

const QSIZE: usize = 8;
const VERSION_1: u32 = 32;
const ENTROPY_LEN: usize = 256;

fn main() {
    let location = DeviceLocation::from_env();
    match run(location) {
        Ok(()) => {}
        Err(error) => eprintln!("virtio-rngd: stopped ({error:?})"),
    }
}

fn run(location: DeviceLocation) -> Result<(), PlatformError> {
    let mut platform = RedoxPlatform::new(location.irq)?;
    let bank = unsafe { platform.map_mmio(location.mmio_phys, location.mmio_len) }?;
    let mmio = unsafe { VirtioMmio::new(bank) }.map_err(|_| PlatformError::Unsupported)?;
    mmio.accept_features(Features::bit(VERSION_1))
        .map_err(|_| PlatformError::Unsupported)?;

    let buf = platform.alloc_dma(ENTROPY_LEN)?;
    let region = platform.alloc_dma(VirtQueue::<QSIZE>::required_bytes())?;
    let queue = unsafe { VirtQueue::<QSIZE>::new(&region) }.map_err(|_| PlatformError::Unsupported)?;
    mmio.setup_queue(0, &queue).map_err(|_| PlatformError::Unsupported)?;
    mmio.set_driver_ok();
    let mut rng = VirtioRng::new(queue);

    // Prime the first request.
    rng.request(&buf).map_err(|_| PlatformError::Unsupported)?;
    mmio.notify(0);

    runtime::run(&mut platform, &mmio, || {
        let mut filled = false;
        while rng.poll().is_some() {
            // `buf` now holds entropy; a full driver feeds it to the RNG pool.
            filled = true;
        }
        if filled {
            let _ = rng.request(&buf);
            mmio.notify(0);
        }
    })
}
