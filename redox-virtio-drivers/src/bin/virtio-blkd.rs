//! `virtio-blkd` — a Redox userspace virtio-blk (storage) driver daemon.
//!
//! Brings the device up, issues a representative read of sector 0 to exercise
//! the data path, then runs the IRQ service loop. A full driver exposes a
//! `disk:` scheme and maps client read/write to blk requests.
//!
//! Build for Redox: `cargo build --release --target x86_64-unknown-redox`.

use driver_primitives::{
    runtime::{self, Platform, PlatformError},
    virtio::{blk, VirtQueue, VirtioBlk, VirtioMmio},
    Features,
};
use redox_virtio_drivers::{DeviceLocation, RedoxPlatform};

const QSIZE: usize = 128;
const VERSION_1: u32 = 32;

fn main() {
    let location = DeviceLocation::from_env();
    match run(location) {
        Ok(()) => {}
        Err(error) => eprintln!("virtio-blkd: stopped ({error:?})"),
    }
}

fn run(location: DeviceLocation) -> Result<(), PlatformError> {
    let mut platform = RedoxPlatform::new(location.irq)?;
    let bank = unsafe { platform.map_mmio(location.mmio_phys, location.mmio_len) }?;
    let mmio = unsafe { VirtioMmio::new(bank) }.map_err(|_| PlatformError::Unsupported)?;
    mmio.accept_features(Features::bit(VERSION_1))
        .map_err(|_| PlatformError::Unsupported)?;

    // Request buffers, allocated before the queue so they outlive it.
    let hdr = platform.alloc_dma(blk::HDR_LEN)?;
    let data = platform.alloc_dma(blk::SECTOR_SIZE)?;
    let status = platform.alloc_dma(1)?;

    let region = platform.alloc_dma(VirtQueue::<QSIZE>::required_bytes())?;
    let queue =
        unsafe { VirtQueue::<QSIZE>::new(&region) }.map_err(|_| PlatformError::Unsupported)?;
    mmio.setup_queue(0, &queue)
        .map_err(|_| PlatformError::Unsupported)?;
    mmio.set_driver_ok();
    let mut disk = VirtioBlk::new(queue);

    // Exercise the data path: read sector 0.
    disk.read(&hdr, &data, &status, 0)
        .map_err(|_| PlatformError::Unsupported)?;
    mmio.notify(0);

    runtime::run(&mut platform, &mmio, || {
        while disk.poll().is_some() {
            // A full driver checks VirtioBlk::status_of(&status) and returns the
            // data to the disk: scheme client here.
        }
    })
}
