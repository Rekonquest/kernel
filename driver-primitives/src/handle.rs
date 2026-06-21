//! Handle table — the dumb substrate under fd / GEM-handle / tag lifecycles.
//!
//! Hands out small opaque [`Handle`]s backed by a fixed slab. Each slot carries
//! a generation counter, so a *stale* handle — one whose slot was freed and
//! later reused for a different object — is reliably rejected instead of
//! silently aliasing the new occupant. That rejection is the safety property fd
//! tables and GPU GEM handle tables depend on; this primitive provides it and
//! nothing else (no naming policy, no refcounting, no permissions).

/// An opaque reference into a [`HandleTable`]. Cheap to copy and store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Handle {
    index: u32,
    generation: u32,
}

impl Handle {
    /// The raw slot index. Exposed for debugging/serialization; prefer passing
    /// the whole `Handle` back to the table.
    pub const fn index(self) -> u32 {
        self.index
    }

    /// The generation this handle was minted at.
    pub const fn generation(self) -> u32 {
        self.generation
    }
}

struct Slot<T> {
    generation: u32,
    value: Option<T>,
}

/// A fixed-capacity table mapping opaque [`Handle`]s to values of type `T`.
pub struct HandleTable<T, const N: usize> {
    slots: [Slot<T>; N],
    /// Stack of free slot indices.
    free: [u32; N],
    free_len: usize,
}

impl<T, const N: usize> Default for HandleTable<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> HandleTable<T, N> {
    /// Create an empty table with all `N` slots free.
    pub fn new() -> Self {
        Self {
            slots: core::array::from_fn(|_| Slot {
                generation: 0,
                value: None,
            }),
            free: core::array::from_fn(|i| i as u32),
            free_len: N,
        }
    }

    /// Total number of slots.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Number of live entries.
    pub const fn len(&self) -> usize {
        N - self.free_len
    }

    /// Whether the table holds no live entries.
    pub const fn is_empty(&self) -> bool {
        self.free_len == N
    }

    /// Whether every slot is in use.
    pub const fn is_full(&self) -> bool {
        self.free_len == 0
    }

    /// Insert `value`, returning a fresh handle. On a full table the value is
    /// handed back via `Err`.
    pub fn alloc(&mut self, value: T) -> Result<Handle, T> {
        if self.free_len == 0 {
            return Err(value);
        }
        self.free_len -= 1;
        let index = self.free[self.free_len];
        let slot = &mut self.slots[index as usize];
        slot.value = Some(value);
        Ok(Handle {
            index,
            generation: slot.generation,
        })
    }

    fn live_slot(&self, handle: Handle) -> Option<&Slot<T>> {
        let slot = self.slots.get(handle.index as usize)?;
        (slot.generation == handle.generation).then_some(slot)
    }

    /// Borrow the value behind `handle`, or `None` if it is stale or unknown.
    pub fn get(&self, handle: Handle) -> Option<&T> {
        self.live_slot(handle)?.value.as_ref()
    }

    /// Mutably borrow the value behind `handle`.
    pub fn get_mut(&mut self, handle: Handle) -> Option<&mut T> {
        let slot = self.slots.get_mut(handle.index as usize)?;
        if slot.generation == handle.generation {
            slot.value.as_mut()
        } else {
            None
        }
    }

    /// Whether `handle` currently refers to a live value.
    pub fn contains(&self, handle: Handle) -> bool {
        self.get(handle).is_some()
    }

    /// Remove and return the value behind `handle`, freeing its slot and
    /// invalidating every outstanding copy of the handle.
    pub fn remove(&mut self, handle: Handle) -> Option<T> {
        let slot = self.slots.get_mut(handle.index as usize)?;
        if slot.generation != handle.generation {
            return None;
        }
        let value = slot.value.take();
        if value.is_some() {
            // Bump the generation so old handles to this slot no longer match.
            slot.generation = slot.generation.wrapping_add(1);
            self.free[self.free_len] = handle.index;
            self.free_len += 1;
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_get_remove_roundtrip() {
        let mut table: HandleTable<u32, 4> = HandleTable::new();
        assert!(table.is_empty());
        let h = table.alloc(99).unwrap();
        assert_eq!(table.len(), 1);
        assert_eq!(table.get(h), Some(&99));
        *table.get_mut(h).unwrap() = 100;
        assert_eq!(table.get(h), Some(&100));
        assert_eq!(table.remove(h), Some(100));
        assert!(table.is_empty());
        assert_eq!(table.get(h), None);
    }

    #[test]
    fn stale_handle_is_rejected_after_remove() {
        let mut table: HandleTable<&str, 2> = HandleTable::new();
        let h = table.alloc("a").unwrap();
        assert_eq!(table.remove(h), Some("a"));
        // The old handle must not resolve, and a second remove is a no-op.
        assert!(!table.contains(h));
        assert_eq!(table.get(h), None);
        assert_eq!(table.remove(h), None);
    }

    #[test]
    fn reused_slot_does_not_alias_old_handle() {
        let mut table: HandleTable<&str, 1> = HandleTable::new();
        let old = table.alloc("first").unwrap();
        table.remove(old).unwrap();
        // Same slot index is reused, but with a bumped generation.
        let new = table.alloc("second").unwrap();
        assert_eq!(old.index(), new.index());
        assert_ne!(old.generation(), new.generation());
        // The stale handle must not see the new occupant.
        assert_eq!(table.get(old), None);
        assert_eq!(table.get(new), Some(&"second"));
    }

    #[test]
    fn full_table_hands_the_value_back() {
        let mut table: HandleTable<u8, 1> = HandleTable::new();
        let _h = table.alloc(1).unwrap();
        assert!(table.is_full());
        assert_eq!(table.alloc(2), Err(2));
    }

    #[test]
    fn forged_handle_into_empty_slot_is_safe() {
        let table: HandleTable<u8, 2> = HandleTable::new();
        // A handle that was never minted must not resolve.
        let forged = Handle {
            index: 0,
            generation: 0,
        };
        assert_eq!(table.get(forged), None);
        let out_of_range = Handle {
            index: 99,
            generation: 0,
        };
        assert_eq!(table.get(out_of_range), None);
    }
}
