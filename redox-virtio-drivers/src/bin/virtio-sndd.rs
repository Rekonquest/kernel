//! `virtio-sndd` — a Redox userspace virtio-snd (audio) driver daemon.
//!
//! Brings the device up with a control queue and a transmit queue and
//! configures PCM stream 0, then runs the IRQ loop. A full driver exposes an
//! `audio:` scheme and submits client PCM buffers on the transmit queue.
//!
//! Build for Redox: `cargo build --release --target x86_64-unknown-redox`.

use driver_primitives::runtime::{self, Platform, PlatformError};
use driver_primitives::virtio::{snd, VirtQueue, VirtioMmio, VirtioSnd};
use driver_primitives::Features;
use redox_virtio_drivers::{DeviceLocation, RedoxPlatform};

const QSIZE: usize = 64;
const VERSION_1: u32 = 32;
const STREAM: u32 = 0;

fn main() {
    let location = DeviceLocation::from_env();
    match run(location) {
        Ok(()) => {}
        Err(error) => eprintln!("virtio-sndd: stopped ({error:?})"),
    }
}

fn run(location: DeviceLocation) -> Result<(), PlatformError> {
    let mut platform = RedoxPlatform::new(location.irq)?;
    let bank = unsafe { platform.map_mmio(location.mmio_phys, location.mmio_len) }?;
    let mmio = unsafe { VirtioMmio::new(bank) }.map_err(|_| PlatformError::Unsupported)?;
    mmio.accept_features(Features::bit(VERSION_1))
        .map_err(|_| PlatformError::Unsupported)?;

    // Command buffers, allocated before the queues so they outlive them.
    let cmd = platform.alloc_dma(snd::SET_PARAMS_LEN)?;
    let resp = platform.alloc_dma(8)?;

    let cregion = platform.alloc_dma(VirtQueue::<QSIZE>::required_bytes())?;
    let tregion = platform.alloc_dma(VirtQueue::<QSIZE>::required_bytes())?;
    let control = unsafe { VirtQueue::<QSIZE>::new(&cregion) }.map_err(|_| PlatformError::Unsupported)?;
    let tx = unsafe { VirtQueue::<QSIZE>::new(&tregion) }.map_err(|_| PlatformError::Unsupported)?;
    mmio.setup_queue(0, &control).map_err(|_| PlatformError::Unsupported)?;
    mmio.setup_queue(1, &tx).map_err(|_| PlatformError::Unsupported)?;
    mmio.set_driver_ok();
    let mut audio = VirtioSnd::new(control, tx);

    // Configure PCM stream 0 (stereo, S16, 48 kHz).
    audio
        .set_params(&cmd, &resp, STREAM, 8192, 2048, 2, snd::format::S16, snd::rate::R48000)
        .map_err(|_| PlatformError::Unsupported)?;
    mmio.notify(0);

    runtime::run(&mut platform, &mmio, || {
        while audio.poll_control().is_some() {}
        while audio.poll_tx().is_some() {}
    })
}
