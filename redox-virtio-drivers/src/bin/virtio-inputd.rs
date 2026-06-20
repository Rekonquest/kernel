//! `virtio-inputd` — a Redox userspace virtio-input (HID) driver daemon.
//!
//! Brings the device up, pre-posts a pool of event buffers, and on each
//! interrupt delivers and re-posts input events. A full driver decodes each
//! event and forwards it to an `input:` scheme.
//!
//! Build for Redox: `cargo build --release --target x86_64-unknown-redox`.

use driver_primitives::dma::DmaRegion;
use driver_primitives::runtime::{self, Platform, PlatformError};
use driver_primitives::virtio::input::{self};
use driver_primitives::virtio::{VirtQueue, VirtioInput, VirtioMmio};
use driver_primitives::Features;
use redox_virtio_drivers::{DeviceLocation, RedoxPlatform};

const QSIZE: usize = 64;
const POOL: usize = 16;
const VERSION_1: u32 = 32;

fn main() {
    let location = DeviceLocation::from_env();
    match run(location) {
        Ok(()) => {}
        Err(error) => eprintln!("virtio-inputd: stopped ({error:?})"),
    }
}

fn run(location: DeviceLocation) -> Result<(), PlatformError> {
    let mut platform = RedoxPlatform::new(location.irq)?;
    let bank = unsafe { platform.map_mmio(location.mmio_phys, location.mmio_len) }?;
    let mmio = unsafe { VirtioMmio::new(bank) }.map_err(|_| PlatformError::Unsupported)?;
    mmio.accept_features(Features::bit(VERSION_1))
        .map_err(|_| PlatformError::Unsupported)?;

    // Event buffers, allocated before the queue so they outlive it.
    let ev_bufs: Vec<_> = (0..POOL)
        .map(|_| platform.alloc_dma(input::EVENT_LEN))
        .collect::<Result<Vec<_>, _>>()?;

    let region = platform.alloc_dma(VirtQueue::<QSIZE>::required_bytes())?;
    let queue = unsafe { VirtQueue::<QSIZE>::new(&region) }.map_err(|_| PlatformError::Unsupported)?;
    mmio.setup_queue(0, &queue).map_err(|_| PlatformError::Unsupported)?;
    mmio.set_driver_ok();

    let descs: [(u64, u32); POOL] =
        core::array::from_fn(|i| (ev_bufs[i].phys_addr(), input::EVENT_LEN as u32));
    let mut device: VirtioInput<QSIZE, POOL> = VirtioInput::new(queue, descs);
    device.start().map_err(|_| PlatformError::Unsupported)?;
    mmio.notify(0);

    runtime::run(&mut platform, &mmio, || {
        let mut delivered = false;
        while let Some((index, _len)) = device.poll() {
            // A full driver decodes input::parse_event(&ev_bufs[index]) and
            // forwards it to the input: scheme before reposting.
            let _ = device.repost(index);
            delivered = true;
        }
        if delivered {
            mmio.notify(0);
        }
    })
}
