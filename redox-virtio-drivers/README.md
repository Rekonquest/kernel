# redox-virtio-drivers

Redox userspace virtio driver daemons built entirely from the portable
[`driver-primitives`](../driver-primitives) kit. This crate is the **OS-specific
edge**: it implements the kit's `Platform` seam for Redox and ships the daemon
binaries. Everything above the seam (transport, virtqueues, virtio-net,
virtio-gpu, the display orchestrator, the reactor) is portable and host-tested
in `driver-primitives`.

## Layout

| File | Role |
|------|------|
| `src/platform.rs` | `RedoxPlatform` — DMA via `physalloc`/`physmap`, IRQ via the `irq` scheme. Real impl behind `cfg(target_os = "redox")`; a stub otherwise. |
| `src/bin/virtio-netd.rs` | virtio-net daemon: map → negotiate → set up RX/TX queues → DRIVER_OK → interrupt-driven service loop. |
| `src/bin/virtio-gpud.rs` | virtio-gpu daemon: bring up the control queue, discover monitors, configure them all in one atomic modeset. |

## Building

- **On any host (type-check only):** `cargo check` — the Redox binding compiles
  to a stub (`PlatformError::Unsupported`) so the daemon logic still
  type-checks. `redox_syscall` is a Redox-target-only dependency and is not
  built here.
- **On Redox:** `cargo build --release --target x86_64-unknown-redox`, normally
  through the Redox build system (a cookbook recipe).

## Bringing it up in QEMU (when ready)

1. Add this crate (and `driver-primitives`) to a Redox checkout as a cookbook
   recipe, or vendor them into `cookbook/recipes/`.
2. Have the bus driver (`pcid`) launch the daemon with the device location —
   the skeleton reads `VIRTIO_IRQ`, `VIRTIO_MMIO_PHYS`, `VIRTIO_MMIO_LEN` from
   the environment; wire these from the bus driver's device info.
3. Add the binaries to the filesystem config and `make qemu`
   (e.g. `-device virtio-net-device` / `-device virtio-gpu-device` for the
   MMIO transport).

## Honesty note

The `cfg(target_os = "redox")` binding targets the `redox_syscall` API at the
pinned revision (`physalloc`/`physmap`/`physunmap`/`physfree`, the `irq`
scheme via `open`/`read`/`write`). It is **not compiled in this sandbox** — only
the daemon logic above the seam is verified here. Expect to adjust a syscall
signature or two against the exact `redox_syscall` version when you first build
on the Redox target. The full driver also still needs: pre-posting RX buffers
and a `network:` scheme (netd), and the page-flip/transfer/flush present loop
wired to a framebuffer scheme (gpud).
