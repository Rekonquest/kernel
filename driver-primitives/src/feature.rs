//! Feature negotiation — capability discovery as a bitset intersection.
//!
//! Every bus settles capabilities the same way: the device advertises what it
//! *offers*, the driver states what it *wants*, and both run on the
//! intersection. virtio feature bits, DRM `GET_CAP`/`SET_CLIENT_CAP`, ethtool
//! NIC features, WiFi cipher/AKM suites, ALSA format/rate masks — all the same
//! AND. This primitive is that AND, plus a check that any *mandatory* features
//! survived it.

/// A set of up to 64 feature bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Features(u64);

impl Features {
    /// The empty set.
    pub const NONE: Features = Features(0);

    /// Build a set from a raw bitmask.
    pub const fn from_bits(bits: u64) -> Self {
        Features(bits)
    }

    /// A single feature by bit index (`index` must be `< 64`).
    pub const fn bit(index: u32) -> Self {
        Features(1u64 << index)
    }

    /// The raw bitmask.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Whether every bit in `other` is also present in `self`.
    pub const fn contains(self, other: Features) -> bool {
        self.0 & other.0 == other.0
    }

    /// Bits present in both sets.
    pub const fn intersection(self, other: Features) -> Features {
        Features(self.0 & other.0)
    }

    /// Bits present in either set.
    pub const fn union(self, other: Features) -> Features {
        Features(self.0 | other.0)
    }

    /// Bits in `self` that are not in `other`.
    pub const fn difference(self, other: Features) -> Features {
        Features(self.0 & !other.0)
    }

    /// Whether the set is empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// Settle on the common subset: what the device `offered` AND the driver
/// `requested`.
pub const fn negotiate(offered: Features, requested: Features) -> Features {
    offered.intersection(requested)
}

/// Like [`negotiate`], but fail if any `required` feature did not survive the
/// intersection. On failure the returned [`Features`] is the set of missing
/// mandatory bits — exactly what a driver needs to log "device lacks X".
pub const fn negotiate_checked(
    offered: Features,
    requested: Features,
    required: Features,
) -> Result<Features, Features> {
    let agreed = offered.intersection(requested);
    let missing = required.difference(agreed);
    if missing.is_empty() {
        Ok(agreed)
    } else {
        Err(missing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate_is_the_intersection() {
        let offered = Features::from_bits(0b0111); // bits 0,1,2
        let requested = Features::from_bits(0b1110); // bits 1,2,3
        assert_eq!(negotiate(offered, requested).bits(), 0b0110); // bits 1,2
    }

    #[test]
    fn contains_checks_all_bits() {
        let set = Features::from_bits(0b1010);
        assert!(set.contains(Features::bit(1)));
        assert!(set.contains(Features::bit(3)));
        assert!(set.contains(Features::from_bits(0b1010)));
        assert!(!set.contains(Features::bit(0)));
        assert!(!set.contains(Features::from_bits(0b1011)));
    }

    #[test]
    fn set_algebra() {
        let a = Features::from_bits(0b1100);
        let b = Features::from_bits(0b0110);
        assert_eq!(a.union(b).bits(), 0b1110);
        assert_eq!(a.intersection(b).bits(), 0b0100);
        assert_eq!(a.difference(b).bits(), 0b1000);
        assert!(Features::NONE.is_empty());
        assert!(!a.is_empty());
    }

    #[test]
    fn required_feature_present_succeeds() {
        let offered = Features::from_bits(0b0111);
        let requested = Features::from_bits(0b1110);
        let required = Features::bit(1); // survives the intersection
        assert_eq!(
            negotiate_checked(offered, requested, required),
            Ok(Features::from_bits(0b0110))
        );
    }

    #[test]
    fn missing_required_feature_reports_what_is_missing() {
        let offered = Features::from_bits(0b0011); // bits 0,1
        let requested = Features::from_bits(0b0001); // bit 0
        let required = Features::bit(1); // bit 1 won't survive (driver didn't request it)
        assert_eq!(
            negotiate_checked(offered, requested, required),
            Err(Features::bit(1))
        );
    }
}
