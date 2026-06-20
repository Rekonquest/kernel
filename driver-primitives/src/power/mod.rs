//! Power management orchestrators — the kit absorbing another driver class.
//!
//! Each slice is thin policy over primitives already proven elsewhere:
//!
//! - [`budget::PowerBudget`] — weighted power/thermal budget allocation, the
//!   allocation cousin of the scheduler's `FairQueue`.
//! - [`states::PowerManager`] — atomic suspend/resume via `Transaction`
//!   (all-or-nothing across devices, a device's veto aborts the whole thing).
//! - [`sensors::Sensors`] — battery/thermal monitoring as an `EventQueue` of
//!   threshold crossings.
//! - [`governor::Governor`] — CPU P-state/C-state selection with hysteresis.

pub mod budget;
pub mod governor;
pub mod sensors;
pub mod states;

pub use budget::PowerBudget;
pub use governor::Governor;
pub use sensors::{SensorEvent, Sensors};
pub use states::{DeviceState, PowerManager, SystemState};
