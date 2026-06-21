# Booting the drivers in Redox / QEMU

This is the turnkey path from the crates in this repo to drivers running on a
Redox boot in QEMU. None of it can run in the cloud dev sandbox (no QEMU, the
Redox build lives on the network-blocked `gitlab.redox-os.org`); it runs on your
machine or in GitHub Actions.

## 0. What's already verified vs. what this step adds

- **Verified off-target (CI + sandbox):** all kit logic and data paths
  (`driver-primitives`, 83 tests), bare-metal build, and the daemon logic
  type-checked on host. The `redox-cross-build` CI job compiles the
  `cfg(target_os = "redox")` binding (`platform.rs`, `scheme.rs`) against the
  real Redox target via `redoxer`.
- **This step adds:** running the drivers against real virtio devices on a boot,
  which validates the syscall binding and the scheme/IRQ event loop end to end.

## 1. Prerequisites

Install the Redox build system (see <https://doc.redox-os.org/book/podman-build.html>):

```sh
git clone https://gitlab.redox-os.org/redox-os/redox.git
cd redox
./bootstrap.sh           # installs the toolchain + dependencies (or use podman)
make pull
```

QEMU comes with the Redox tooling; confirm `qemu-system-x86_64 --version`.

## 2. Add the crates as a cookbook recipe

Vendor `driver-primitives/` and `redox-virtio-drivers/` into the Redox tree (or
point a recipe at this git repo), then create
`cookbook/recipes/virtio-drivers/recipe.toml`:

```toml
[source]
git = "https://github.com/Rekonquest/kernel.git"
branch = "claude/wonderful-gates-r4rf6o"

[build]
template = "custom"
script = "cargo build --release --manifest-path redox-virtio-drivers/Cargo.toml"
```

Install the six binaries (`virtio-netd`, `virtio-gpud`, `virtio-blkd`,
`virtio-rngd`, `virtio-inputd`, `virtio-sndd`) into `/usr/bin` via the recipe's
package stage, and add `virtio-drivers` to your filesystem config
(`config/x86_64/*.toml`, under `[packages]`).

## 3. Hand each daemon its device

The daemons read their device location from the environment:
`VIRTIO_IRQ`, `VIRTIO_MMIO_PHYS`, `VIRTIO_MMIO_LEN` (see `DeviceLocation::from_env`).
In a real system the bus driver (`pcid` for virtio-pci, or a device-tree walk
for virtio-mmio) discovers these and launches the matching daemon. For a first
bring-up you can launch a daemon directly with the values for a `-device
virtio-*-device` (virtio-mmio) instance.

## 4. Finish the event loop (the one piece needing the live API)

Each daemon currently brings the device up and runs `runtime::run` on the IRQ.
For full scheme service, replace that with a loop that multiplexes the **scheme
socket** and the **device IRQ** via the `event:` scheme:

```text
socket = open(":network", O_RDWR | O_CREAT | O_CLOEXEC)
event  = open("event:", O_RDWR | O_CLOEXEC)
write(event, Event { id: socket, flags: EVENT_READ })
write(event, Event { id: irq_fd, flags: EVENT_READ })
loop {
    read(event, &mut events)
    for e in events {
        if e.id == irq_fd  { service device: poll completions, RxPool -> NetScheme::deliver, ack IRQ }
        if e.id == socket  { read Packet, NetScheme::handle, write Packet }
    }
    // drain NetScheme::take_tx() -> VirtioNet::transmit(); notify the queue
}
```

`scheme.rs` already holds the data-path queues (`NetScheme::deliver` /
`take_tx`, `FbScheme::take_flips`); this loop is the only Redox-specific glue
left, and it is what the boot validates.

## 5. Run

```sh
make qemu \
  QEMU_EXTRA="-device virtio-net-device \
              -device virtio-gpu-device \
              -device virtio-blk-device,drive=disk \
              -device virtio-rng-device \
              -device virtio-keyboard-device \
              -device virtio-sound-device"
```

(Exact device names depend on your QEMU version; `virtio-*-device` are the
virtio-mmio variants, `virtio-*-pci` the PCI ones — match the transport your
daemons map.)

Watch the daemons come up in the Redox log, then exercise them: `ping` over
`network:`, a window on the GPU scanouts, `cat` a file off the virtio-blk disk.

## 6. Fast inner loop without a full image

To iterate on just the Redox cross-build (catches `redox_syscall` API drift
before a full boot):

```sh
cargo install redoxer
redoxer toolchain
cd redox-virtio-drivers && redoxer build --release
```

This is exactly what the `redox-cross-build` CI job runs.
