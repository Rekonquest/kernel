//! System suspend/resume as an atomic, validated device-state transition.
//!
//! Suspending is the multi-screen modeset pattern again: it must be
//! all-or-nothing across every device. If any device vetoes the transition
//! (e.g. a busy device that blocks suspend), the whole thing aborts and nothing
//! changes — exactly [`Transaction::validate`] then [`Transaction::commit`].
//!
//! [`Transaction::validate`]: crate::txn::Transaction::validate
//! [`Transaction::commit`]: crate::txn::Transaction::commit

use crate::txn::Transaction;

/// ACPI-style device power state, `D0` (full on) through `D3` (off).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceState {
    D0,
    D1,
    D2,
    D3,
}

/// System power state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemState {
    /// Working.
    S0,
    /// Suspend-to-RAM.
    S3,
    /// Soft off.
    S5,
}

/// Returned when a device vetoes a suspend, leaving the system untouched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SuspendVetoed;

#[derive(Clone, Copy)]
struct Device {
    active: bool,
    state: DeviceState,
    blocks_suspend: bool,
}

impl Device {
    const EMPTY: Device = Device {
        active: false,
        state: DeviceState::D0,
        blocks_suspend: false,
    };
}

/// Manages the power state of up to `N` devices and the system as a whole.
pub struct PowerManager<const N: usize> {
    devices: [Device; N],
    system: SystemState,
}

impl<const N: usize> Default for PowerManager<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> PowerManager<N> {
    /// Create a manager with no devices, system in `S0`.
    pub fn new() -> Self {
        Self {
            devices: [Device::EMPTY; N],
            system: SystemState::S0,
        }
    }

    /// Register device `id`. `blocks_suspend` marks a device that will veto a
    /// suspend while busy.
    pub fn register(&mut self, id: usize, blocks_suspend: bool) -> bool {
        match self.devices.get_mut(id) {
            Some(d) => {
                d.active = true;
                d.state = DeviceState::D0;
                d.blocks_suspend = blocks_suspend;
                true
            }
            None => false,
        }
    }

    /// Set whether device `id` currently blocks suspend (e.g. mid-transfer).
    pub fn set_blocks_suspend(&mut self, id: usize, blocks: bool) {
        if let Some(d) = self.devices.get_mut(id)
            && d.active
        {
            d.blocks_suspend = blocks;
        }
    }

    /// The current power state of device `id`.
    pub fn device_state(&self, id: usize) -> Option<DeviceState> {
        self.devices.get(id).filter(|d| d.active).map(|d| d.state)
    }

    /// The current system power state.
    pub fn system_state(&self) -> SystemState {
        self.system
    }

    /// Suspend to `S3`: atomically move every device to `D3`. If any active
    /// device blocks suspend, the transition aborts and nothing changes.
    pub fn suspend(&mut self) -> Result<(), SuspendVetoed> {
        if self.system != SystemState::S0 {
            return Ok(());
        }
        let mut txn: Transaction<usize, N> = Transaction::new();
        for id in 0..N {
            if self.devices[id].active {
                let _ = txn.stage(id);
            }
        }
        if !txn.validate(|&id| !self.devices[id].blocks_suspend) {
            return Err(SuspendVetoed); // a device vetoed — nothing applied
        }
        txn.commit(|id| self.devices[id].state = DeviceState::D3);
        self.system = SystemState::S3;
        Ok(())
    }

    /// Resume to `S0`: bring every device back to `D0`.
    pub fn resume(&mut self) {
        if self.system != SystemState::S3 {
            return;
        }
        for d in self.devices.iter_mut() {
            if d.active {
                d.state = DeviceState::D0;
            }
        }
        self.system = SystemState::S0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspend_and_resume_move_every_device() {
        let mut pm: PowerManager<4> = PowerManager::new();
        pm.register(0, false);
        pm.register(1, false);
        pm.register(2, false);

        assert!(pm.suspend().is_ok());
        assert_eq!(pm.system_state(), SystemState::S3);
        for id in 0..3 {
            assert_eq!(pm.device_state(id), Some(DeviceState::D3));
        }

        pm.resume();
        assert_eq!(pm.system_state(), SystemState::S0);
        for id in 0..3 {
            assert_eq!(pm.device_state(id), Some(DeviceState::D0));
        }
    }

    #[test]
    fn a_blocking_device_aborts_the_whole_suspend() {
        let mut pm: PowerManager<4> = PowerManager::new();
        pm.register(0, false);
        pm.register(1, true); // device 1 is busy and blocks suspend
        pm.register(2, false);

        assert!(pm.suspend().is_err());
        // Nothing moved: every device is still D0 and the system is still S0.
        assert_eq!(pm.system_state(), SystemState::S0);
        for id in 0..3 {
            assert_eq!(pm.device_state(id), Some(DeviceState::D0));
        }

        // Once it stops blocking, suspend succeeds.
        pm.set_blocks_suspend(1, false);
        assert!(pm.suspend().is_ok());
        assert_eq!(pm.device_state(1), Some(DeviceState::D3));
    }
}
