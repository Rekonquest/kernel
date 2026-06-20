//! `virtio-gpud` — a Redox userspace virtio-gpu driver daemon (multi-screen).
//!
//! Brings up the control queue, discovers the monitors, configures them all in
//! one atomic modeset, sets up scanout 0 with a framebuffer, and presents an
//! initial frame; then runs the IRQ loop draining completions. The device logic
//! is `driver-primitives`; this file is the Redox wiring. A framebuffer scheme
//! letting clients draw and request flips is the remaining glue (`src/scheme.rs`).
//!
//! Build for Redox: `cargo build --release --target x86_64-unknown-redox`.

use driver_primitives::{
    display::{Change, Display, Mode},
    dma::DmaRegion,
    runtime::{self, Platform, PlatformError},
    txn::Transaction,
    virtio::{gpu, Rect, VirtQueue, VirtioGpu, VirtioMmio},
    Features,
};
use redox_virtio_drivers::{DeviceLocation, RedoxPlatform};

const QSIZE: usize = 64;
const VERSION_1: u32 = 32; // VIRTIO_F_VERSION_1
const FB_RESOURCE: u32 = 1;

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

    // Control queue (queue 0); its region outlives the queue.
    let ctrl_region = platform.alloc_dma(VirtQueue::<QSIZE>::required_bytes())?;
    let ctrl =
        unsafe { VirtQueue::<QSIZE>::new(&ctrl_region) }.map_err(|_| PlatformError::Unsupported)?;
    mmio.setup_queue(0, &ctrl)
        .map_err(|_| PlatformError::Unsupported)?;
    mmio.set_driver_ok();
    let mut gpu_dev = VirtioGpu::new(ctrl);

    // Command/response and present buffers, kept alive for the daemon's life.
    let cmd = platform.alloc_dma(gpu::CMD_MAX_LEN)?;
    let resp = platform.alloc_dma(gpu::RESP_DISPLAY_INFO_LEN)?;
    let tcmd = platform.alloc_dma(gpu::CMD_MAX_LEN)?;
    let tresp = platform.alloc_dma(gpu::RESP_NODATA_LEN)?;

    // 1. Discover monitors (synchronous bring-up).
    gpu_dev
        .get_display_info(&cmd, &resp)
        .map_err(|_| PlatformError::Unsupported)?;
    wait_completion(&mut platform, &mmio, &mut gpu_dev)?;
    let (scanouts, _count) = gpu::parse_display_info(&resp);

    // 2. Configure every reported monitor in one atomic modeset.
    let mut display: Display<{ gpu::MAX_SCANOUTS }> = Display::new();
    let mut txn: Transaction<Change, 32> = Transaction::new();
    for (output, scanout) in scanouts.iter().enumerate() {
        if scanout.enabled {
            let mode = Mode::new(scanout.rect.width, scanout.rect.height, 60_000);
            display.connect(output, mode);
            let _ = txn.stage(Change::SetMode { output, mode });
        }
    }
    let _ = display.commit(txn);

    // 3. Set up scanout 0 with a framebuffer (the fb region outlives the device).
    let primary = scanouts[0];
    let (w, h) = (primary.rect.width.max(1), primary.rect.height.max(1));
    let fb = platform.alloc_dma((w as usize) * (h as usize) * 4)?;
    let rect = Rect::new(0, 0, w, h);

    gpu_dev
        .create_resource_2d(&cmd, &resp, FB_RESOURCE, gpu::format::B8G8R8A8_UNORM, w, h)
        .map_err(|_| PlatformError::Unsupported)?;
    wait_completion(&mut platform, &mmio, &mut gpu_dev)?;
    gpu_dev
        .attach_backing(&cmd, &resp, FB_RESOURCE, fb.phys_addr(), fb.len() as u32)
        .map_err(|_| PlatformError::Unsupported)?;
    wait_completion(&mut platform, &mmio, &mut gpu_dev)?;
    gpu_dev
        .set_scanout(&cmd, &resp, 0, FB_RESOURCE, rect)
        .map_err(|_| PlatformError::Unsupported)?;
    wait_completion(&mut platform, &mmio, &mut gpu_dev)?;

    // 4. Present the initial frame (transfer + flush).
    gpu_dev
        .present(&cmd, &resp, &tcmd, &tresp, FB_RESOURCE, rect, 0)
        .map_err(|_| PlatformError::Unsupported)?;

    // 5. Service loop: drain completions on each interrupt. A framebuffer scheme
    //    would call present() again whenever a client requests a flip.
    runtime::run(&mut platform, &mmio, || while gpu_dev.poll().is_some() {})
}

/// Block until one control-queue command completes (used during synchronous
/// bring-up, before the steady-state service loop).
fn wait_completion<const Q: usize>(
    platform: &mut RedoxPlatform,
    mmio: &VirtioMmio,
    gpu_dev: &mut VirtioGpu<Q>,
) -> Result<(), PlatformError> {
    mmio.notify(0);
    loop {
        platform.wait_irq()?;
        let causes = mmio.interrupt_status();
        let completed = gpu_dev.poll().is_some();
        mmio.ack_interrupt(causes);
        platform.ack_irq()?;
        if completed {
            return Ok(());
        }
    }
}
