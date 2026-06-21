//! Battery and thermal monitoring — sensor readings as a threshold event stream.
//!
//! Readings are pushed in; threshold *crossings* (low/critical battery, thermal
//! warning/critical) come out as events on an [`EventQueue`]. Events are
//! edge-triggered — a crossing fires once and re-arms only after the reading
//! recovers past the threshold — so a steady alarm does not flood the stream.
//!
//! [`EventQueue`]: crate::event::EventQueue

use crate::event::EventQueue;

/// A monitoring event from crossing a threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SensorEvent {
    /// Battery dropped to/below the low threshold (percent).
    LowBattery(u8),
    /// Battery dropped to/below the critical threshold (percent).
    CriticalBattery(u8),
    /// Temperature rose to/above the warning threshold (°C).
    ThermalWarning(i16),
    /// Temperature rose to/above the critical threshold (°C).
    ThermalCritical(i16),
}

/// Battery + thermal monitor with an event queue of capacity `N`.
pub struct Sensors<const N: usize> {
    low_batt: u8,
    crit_batt: u8,
    warn_temp: i16,
    crit_temp: i16,
    in_low: bool,
    in_crit_batt: bool,
    in_warn: bool,
    in_crit_temp: bool,
    events: EventQueue<SensorEvent, N>,
}

impl<const N: usize> Sensors<N> {
    /// Create a monitor with the given battery (percent) and temperature (°C)
    /// thresholds.
    pub const fn new(low_batt: u8, crit_batt: u8, warn_temp: i16, crit_temp: i16) -> Self {
        Self {
            low_batt,
            crit_batt,
            warn_temp,
            crit_temp,
            in_low: false,
            in_crit_batt: false,
            in_warn: false,
            in_crit_temp: false,
            events: EventQueue::new(),
        }
    }

    /// Feed a battery reading (percent), emitting crossing events. The outer
    /// (low) threshold is checked before the inner (critical) one, so a sudden
    /// drop past both reports low first.
    pub fn update_battery(&mut self, pct: u8) {
        if pct <= self.low_batt {
            if !self.in_low {
                self.in_low = true;
                let _ = self.events.post(SensorEvent::LowBattery(pct));
            }
        } else {
            self.in_low = false;
        }

        if pct <= self.crit_batt {
            if !self.in_crit_batt {
                self.in_crit_batt = true;
                let _ = self.events.post(SensorEvent::CriticalBattery(pct));
            }
        } else {
            self.in_crit_batt = false;
        }
    }

    /// Feed a temperature reading (°C), emitting crossing events. The outer
    /// (warning) threshold is checked before the inner (critical) one, so a
    /// sudden rise past both reports the warning first.
    pub fn update_temp(&mut self, celsius: i16) {
        if celsius >= self.warn_temp {
            if !self.in_warn {
                self.in_warn = true;
                let _ = self.events.post(SensorEvent::ThermalWarning(celsius));
            }
        } else {
            self.in_warn = false;
        }

        if celsius >= self.crit_temp {
            if !self.in_crit_temp {
                self.in_crit_temp = true;
                let _ = self.events.post(SensorEvent::ThermalCritical(celsius));
            }
        } else {
            self.in_crit_temp = false;
        }
    }

    /// Take the next pending sensor event, if any.
    pub fn poll(&mut self) -> Option<SensorEvent> {
        self.events.poll()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_crossings_are_edge_triggered() {
        let mut s: Sensors<8> = Sensors::new(20, 5, 80, 95);
        s.update_battery(100); // fine
        assert_eq!(s.poll(), None);
        s.update_battery(15); // crosses low
        assert_eq!(s.poll(), Some(SensorEvent::LowBattery(15)));
        s.update_battery(12); // still low -> no repeat
        assert_eq!(s.poll(), None);
        s.update_battery(4); // crosses critical
        assert_eq!(s.poll(), Some(SensorEvent::CriticalBattery(4)));
        s.update_battery(50); // recovers (re-arms)
        assert_eq!(s.poll(), None);
        s.update_battery(15); // crosses low again
        assert_eq!(s.poll(), Some(SensorEvent::LowBattery(15)));
    }

    #[test]
    fn thermal_crossings_fire_on_the_way_up() {
        let mut s: Sensors<8> = Sensors::new(20, 5, 80, 95);
        s.update_temp(70);
        assert_eq!(s.poll(), None);
        s.update_temp(85);
        assert_eq!(s.poll(), Some(SensorEvent::ThermalWarning(85)));
        s.update_temp(96);
        assert_eq!(s.poll(), Some(SensorEvent::ThermalCritical(96)));
        s.update_temp(60); // cool down, re-arm
        assert_eq!(s.poll(), None);
        s.update_temp(96); // warning fires before critical on the way up
        assert_eq!(s.poll(), Some(SensorEvent::ThermalWarning(96)));
        assert_eq!(s.poll(), Some(SensorEvent::ThermalCritical(96)));
    }
}
