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
//!
//! GPU, USB, WiFi, ethernet, audio, and storage are not six problems; they are
//! six *orchestrations* of one kit. Build the kit once, dumb and tested; each
//! driver becomes a thin policy layer on top.
//!
//! ## Portability
//!
//! Nothing here depends on a specific OS, allocator, or hardware. The primitives
//! are `no_std`, allocation-free (fixed capacity via const generics), and hold
//! no locks — synchronization and the OS-facing glue (MMIO/DMA mapping, IRQ
//! delivery) belong to the orchestrator that wraps them.

pub mod fence;
pub mod ring;

pub use fence::{Fence, SeqCounter};
pub use ring::{Full, Ring};

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
}
