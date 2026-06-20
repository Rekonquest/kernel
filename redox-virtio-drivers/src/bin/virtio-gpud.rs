//! `virtio-gpud` — a Redox userspace virtio-gpu driver daemon (multi-screen).
//!
//! Maps the device, brings up the control queue, asks for the display layout,
//! and configures every reported monitor in one atomic modeset via the kit's
//! display orchestrator. The device logic is `driver-primitives`; this file is
//! the Redox wiring.
//!
//! Build for Redox: `cargo build --release --target x86_64-unknown-redox`.

use driver_primitives::display::{Change, Display, Mode};
use driver_primitives::runtime::{self, Platform, PlatformError};
use driver_primitives::txn::Transaction;
use driver_primitives::virtio::{gpu, VirtQueue, VirtioGpu, VirtioMmio};
use driver_primitives::Features;
use redox_virtio_drivers::{DeviceLocation, RedoxPlatform};

const QSIZE: usize = 64;
/// VIRTIO_F_VERSION_1 — mandatory for modern virtio.
const VERSION_1: u32 = 32;

fn main() {
    let location = DeviceLocation::from_env();
    match run(location) {
        Ok(()) => {}
        Err(error) => eprintln!("virtio-gpud: stopped ({error:?})"),
    }
}

fn run(location: DeviceLocation) -> Result<(), PlatformError> {
    let mut platform = RedoxPlatform::new(location.irq)?;

    let bank = unsafe { platform.map_mmio(location.mmio_phys, location.mmio_len) }?;
    let mmio = unsafe { VirtioMmio::new(bank) }.map_err(|_| PlatformError::Unsupported)?;
    mmio.accept_features(Features::bit(VERSION_1))
        .map_err(|_| PlatformError::Unsupported)?;

    // Control queue (queue 0); its backing region outlives the queue.
    let ctrl_region = platform.alloc_dma(VirtQueue::<QSIZE>::required_bytes())?;
    let ctrl = unsafe { VirtQueue::<QSIZE>::new(&ctrl_region) }.map_err(|_| PlatformError::Unsupported)?;
    mmio.setup_queue(0, &ctrl).map_err(|_| PlatformError::Unsupported)?;
    mmio.set_driver_ok();

    let mut gpu_dev = VirtioGpu::new(ctrl);

    // Ask the device what monitors exist.
    let cmd = platform.alloc_dma(gpu::CMD_MAX_LEN)?;
    let resp = platform.alloc_dma(gpu::RESP_DISPLAY_INFO_LEN)?;
    gpu_dev
        .get_display_info(&cmd, &resp)
        .map_err(|_| PlatformError::Unsupported)?;

    // On the display-info completion, configure every reported monitor at once.
    let mut display: Display<{ gpu::MAX_SCANOUTS }> = Display::new();
    let mut configured = false;

    runtime::run(&mut platform, &mmio, || {
        while gpu_dev.poll().is_some() {
            if configured {
                continue;
            }
            let (scanouts, _count) = gpu::parse_display_info(&resp);
            let mut txn: Transaction<Change, 32> = Transaction::new();
            for (output, scanout) in scanouts.iter().enumerate() {
                if scanout.enabled {
                    let mode = Mode::new(scanout.rect.width, scanout.rect.height, 60_000);
                    display.connect(output, mode);
                    let _ = txn.stage(Change::SetMode { output, mode });
                }
            }
            // Bring every monitor up atomically; ignore the result in the skeleton.
            let _ = display.commit(txn);
            configured = true;
        }
    })
}
