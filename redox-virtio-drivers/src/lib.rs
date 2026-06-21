//! Redox userspace virtio driver daemons, built from `driver-primitives`.
//!
//! This crate is the OS-specific edge: it implements the kit's
//! [`Platform`](driver_primitives::runtime::Platform) seam for Redox (DMA via
//! `physalloc`/`physmap`, interrupts via the `irq` scheme) and ships the daemon
//! binaries (`virtio-netd`, `virtio-gpud`) that wire a mapped device up to the
//! kit's transport, queues, and reactor. Everything above the seam lives in the
//! portable, tested `driver-primitives` crate; only the thin binding here is
//! Redox-specific.
//!
//! Off Redox the binding compiles to a stub so the daemon logic still
//! type-checks; on Redox build with `--target x86_64-unknown-redox`.

pub mod platform;

/// Scheme front-ends (data-path queues). OS-agnostic and host-tested; the
/// scheme server that drives them is Redox-specific (see BOOTING.md).
pub mod scheme;

pub use platform::{RedoxDma, RedoxPlatform};

/// Device-location parameters a bus driver (e.g. pcid) hands a virtio daemon.
#[derive(Clone, Copy, Debug)]
pub struct DeviceLocation {
    /// Interrupt line.
    pub irq: u8,
    /// Physical base of the device's MMIO register window.
    pub mmio_phys: u64,
    /// Length of the MMIO window.
    pub mmio_len: usize,
}

impl DeviceLocation {
    /// Read a device location from environment variables
    /// (`VIRTIO_IRQ`, `VIRTIO_MMIO_PHYS`, `VIRTIO_MMIO_LEN`). In a real Redox
    /// deployment these come from the bus driver instead.
    pub fn from_env() -> Self {
        fn var(name: &str) -> Option<String> {
            std::env::var(name).ok()
        }
        fn parse_u64(s: &str) -> Option<u64> {
            if let Some(hex) = s.strip_prefix("0x") {
                u64::from_str_radix(hex, 16).ok()
            } else {
                s.parse().ok()
            }
        }
        DeviceLocation {
            irq: var("VIRTIO_IRQ").and_then(|s| s.parse().ok()).unwrap_or(0),
            mmio_phys: var("VIRTIO_MMIO_PHYS")
                .and_then(|s| parse_u64(&s))
                .unwrap_or(0),
            mmio_len: var("VIRTIO_MMIO_LEN")
                .and_then(|s| parse_u64(&s))
                .map(|v| v as usize)
                .unwrap_or(0x1000),
        }
    }
}
