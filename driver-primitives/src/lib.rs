#![cfg_attr(not(test), no_std)]

//! # `driver-primitives` — the dumb-mechanism kit
//!
//! A small set of **primitives** (mechanism, zero decisions) that device
//! drivers compose. Each primitive does one thing, has tight invariants, and
//! takes no policy: it is dumb on purpose. The *driver* is the **orchestrator**
//! — a state machine plus placement/timing policy that drives these primitives.
//!
//! ## Why a kit, not a driver
//!
//! Surveying the published device ABIs (the interface *contracts* — not the
//! implementations) shows the same handful of mechanisms under every class of
//! driver. They differ in policy, not in machinery:
//!
//! | Primitive | Appears as |
//! |-----------|------------|
//! | [`Ring`]  | virtio avail/used · USB URB submit/reap · NVMe SQ/CQ · GPU command IB · V4L2/ALSA QBUF/DQBUF |
//! | [`Fence`] + [`SeqCounter`] | GPU timeline syncobj · command-completion sequence numbers · "has progress reached N?" |
//! | [`HandleTable`] | fd table · GPU GEM handles · USB interface claims |
//! | [`Features`] | virtio feature bits · DRM caps · ethtool NIC features · WiFi cipher/AKM suites · ALSA format/rate masks |
//! | [`EventQueue`] | eventfd/signalfd/timerfd/inotify · DRM events · GPIO line events · netlink async |
//! | [`Transaction`] | DRM atomic modeset · any validate-then-commit batch |
//! | [`FairQueue`] | **cross-domain:** CPU sched (CFS/EEVDF) · GPU ctx priority · NIC flow WFQ · NVMe weighted queueing · audio mixing · TDMA |
//! | [`CapabilityRing`] | **cross-domain:** dma-buf/PRIME · io_uring SQE · Wayland buffers · Redox scheme fd-passing |
//! | [`Reactor`] | **the kit assembled:** io_uring-shaped submit/dispatch/complete/reap engine for any contended async resource |
//! | [`mmio::Reg`] / [`dma::Descriptor`] | **hardware seam:** volatile registers · device-visible DMA buffers (virtio-compatible) |
//! | [`Display`] | **multi-screen:** atomic multi-output modeset (= `Transaction`) · page-flip/vblank (= `Fence`) |
//!
//! GPU, USB, WiFi, ethernet, audio, and storage are not six problems; they are
//! six *orchestrations* of one kit. Build the kit once, dumb and tested; each
//! driver becomes a thin policy layer on top.
//!
//! ## Hardware seam
//!
//! [`mmio`] (volatile registers) and [`dma`] (DMA regions + descriptors) are
//! where the kit meets hardware, and they stay portable on purpose. [`dma::DmaRegion`]
//! is a trait the OS satisfies — on Redox with `redox_syscall::Dma` — so the kit
//! itself depends on no platform. The orchestrator maps an MMIO window (Redox's
//! `memory` scheme) and hands its base to [`mmio::Bank`]; everything above stays
//! the same dumb, tested machinery.
//!
//! ## Cross-domain
//!
//! [`FairQueue`] is the kit reaching past drivers entirely: the same
//! weighted virtual-time fairness the *kernel scheduler* uses to share the CPU
//! is, structurally, what a NIC uses to share bandwidth and a GPU uses to share
//! its engines. Lifting that math out of any one domain into a dumb, reusable
//! arbiter is the doctrine's functional-completeness claim made literal — one
//! small primitive spanning a space far larger than its origin suggests.
//!
//! ## Portability
//!
//! Nothing here depends on a specific OS, allocator, or hardware. The primitives
//! are `no_std`, allocation-free (fixed capacity via const generics), and hold
//! no locks — synchronization and the OS-facing glue (MMIO/DMA mapping, IRQ
//! delivery) belong to the orchestrator that wraps them.

pub mod authority;
pub mod capability;
pub mod display;
pub mod dma;
pub mod event;
pub mod fairqueue;
pub mod feature;
pub mod fence;
pub mod handle;
pub mod mmio;
pub mod power;
pub mod reactor;
pub mod ring;
pub mod runtime;
pub mod txn;
pub mod virtio;

pub use authority::{AtCapacity, Authority, AuthorityWall, CapToken, DelegateError, DomainId};
pub use capability::{CapabilityRing, Grant};
pub use display::{Display, Mode};
pub use dma::{Descriptor, DmaRegion};
pub use event::EventQueue;
pub use fairqueue::FairQueue;
pub use feature::Features;
pub use fence::{Fence, SeqCounter};
pub use handle::{Handle, HandleTable};
pub use mmio::{Bank, Reg};
pub use power::{Governor, PowerBudget, PowerManager, SensorEvent, Sensors};
pub use reactor::{Completion, Reactor, SubmissionId, Ticket};
pub use ring::{Full, Ring};
pub use runtime::{Platform, PlatformError};
pub use txn::Transaction;

#[cfg(test)]
mod tests {
    //! Integration test: the universal submit → complete → wait loop, built
    //! purely from the kit, with a stand-in "device" on the other side.
    use super::*;

    #[test]
    fn submit_complete_wait_loop() {
        // Submission side: a ring of work descriptors, each tagged with a
        // monotonically increasing sequence point.
        let mut submit: Ring<(u64, u32), 4> = Ring::new();
        let mut seq = SeqCounter::new();
        let mut completion = Fence::new();

        // Driver submits three jobs (payloads 10, 20, 30).
        let mut tags = [0u64; 3];
        for (i, payload) in [10u32, 20, 30].into_iter().enumerate() {
            let tag = seq.next_point();
            tags[i] = tag;
            submit.submit((tag, payload)).expect("ring has room");
        }
        assert_eq!(submit.len(), 3);

        // Nothing is done yet.
        assert!(!completion.is_passed(tags[0]));

        // "Device" reaps jobs in order and signals completion of each tag.
        while let Some((tag, _payload)) = submit.reap() {
            assert!(completion.signal(tag));
        }
        assert!(submit.is_empty());

        // Driver can now confirm every job finished.
        for tag in tags {
            assert!(completion.is_passed(tag));
        }
        // And a never-issued future point is still pending.
        assert!(!completion.is_passed(seq.peek() + 1));
    }

    #[test]
    fn device_registry_negotiates_and_emits_events() {
        // 1. Negotiate features: device offers {0,1,2}, driver wants {1,2,3},
        //    and feature 1 is mandatory.
        let offered = Features::from_bits(0b0111);
        let requested = Features::from_bits(0b1110);
        let required = Features::bit(1);
        let agreed =
            feature::negotiate_checked(offered, requested, required).expect("feature 1 survived");
        assert_eq!(agreed.bits(), 0b0110);

        // 2. Register two devices; keep them addressable by opaque handle.
        let mut devices: HandleTable<&str, 4> = HandleTable::new();
        let nic = devices.alloc("nic0").unwrap();
        let gpu = devices.alloc("gpu0").unwrap();

        // 3. Emit completion events tagged by the device handle. The queue
        //    holds two; the third overflows and is counted, not lost silently.
        let mut events: EventQueue<(Handle, u8), 2> = EventQueue::new();
        assert!(events.post((nic, 1)));
        assert!(events.post((gpu, 1)));
        assert!(!events.post((nic, 2)));
        assert_eq!(events.lost(), 1);

        // 4. Drain and resolve handles back to devices.
        let mut buf = [(nic, 0u8); 4];
        let n = events.drain_into(&mut buf);
        assert_eq!(n, 2);
        assert_eq!(devices.get(buf[0].0), Some(&"nic0"));
        assert_eq!(devices.get(buf[1].0), Some(&"gpu0"));

        // 5. Tear down a device; its handle goes stale immediately.
        assert_eq!(devices.remove(nic), Some("nic0"));
        assert_eq!(devices.get(nic), None);
        assert_eq!(devices.get(gpu), Some(&"gpu0"));
    }
}
