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
| `src/bin/virtio-netd.rs` | virtio-net: map → negotiate → set up RX/TX queues → **pre-post an RX buffer pool** → DRIVER_OK → interrupt loop draining TX completions and received frames (re-posting buffers). |
| `src/bin/virtio-gpud.rs` | virtio-gpu: bring up the control queue, discover monitors, configure them all in one atomic modeset, **set up scanout 0 with a framebuffer and present an initial frame**, then run the IRQ loop. |
| `src/bin/virtio-blkd.rs` | virtio-blk (storage): bring up the request queue, read sector 0, run the IRQ loop. |
| `src/bin/virtio-rngd.rs` | virtio-rng (entropy): request entropy and re-request on completion. |
| `src/bin/virtio-inputd.rs` | virtio-input (HID): pre-post event buffers, deliver/re-post events. |
| `src/bin/virtio-sndd.rs` | virtio-snd (audio): control + transmit queues, configure PCM stream 0. |
| `src/scheme.rs` | `network:` / framebuffer **scheme server skeletons** (Redox-only) — the OS-facing glue clients talk to. |

All six daemons follow the same shape: construct `RedoxPlatform`, map the device,
bring it up via the kit, drive its data path, and run `runtime::run` on the IRQ.

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

The `cfg(target_os = "redox")` code (`platform.rs`'s binding and `scheme.rs`'s
servers) is **not compiled in this sandbox** — only the daemon logic above the
seam is verified here. It targets the `redox_syscall` API at the pinned revision
(`physalloc`/`physmap`/`physunmap`/`physfree`, the `irq` scheme, and
`SchemeMut`/`Packet`); expect to adjust a signature or two against the exact
version on first build for the Redox target.

The device data paths are now in place and host-tested in `driver-primitives`:
netd pre-posts an RX buffer pool and drains/re-posts on every interrupt; gpud
sets up a scanout and presents (transfer + flush). What remains is wiring the
**scheme servers** in `scheme.rs` to those data paths (RX frames in/out for
`network:`, flip requests for the framebuffer) and multiplexing the scheme
socket with the device IRQ via the `event:` scheme.
