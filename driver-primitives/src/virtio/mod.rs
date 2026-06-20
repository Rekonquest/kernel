//! virtio orchestration built on the kit.
//!
//! This is not a primitive — it is an *orchestrator* assembled from the kit and
//! the hardware seam ([`crate::dma`], [`crate::mmio`]). [`queue::VirtQueue`] is a
//! faithful split virtqueue with the real virtio memory layout; a device driver
//! (virtio-net, virtio-blk, virtio-gpu) is then policy on top of it plus the
//! [`Reactor`](crate::reactor::Reactor) for submission/completion tracking.

pub mod net;
pub mod queue;
pub mod transport;

pub use net::{VirtioNet, VirtioNetHdr};
pub use queue::{QueueError, Segment, Used, VirtQueue, MAX_CHAIN};
pub use transport::{TransportError, VirtioMmio};
