//! Staged transaction — validate-then-commit, all-or-nothing.
//!
//! The DRM atomic-modeset shape, made generic: accumulate a batch of proposed
//! changes, validate the whole batch with no side effects (the `TEST_ONLY`
//! pass), then either commit it as a unit or abort. Nothing is applied until
//! commit, so a batch that fails validation leaves the target untouched.
//!
//! The primitive owns the staging buffer and the all-or-nothing guarantee; what
//! a "change" *is* and how it applies are the orchestrator's job.

/// A bounded batch of staged changes of type `C`.
pub struct Transaction<C, const N: usize> {
    staged: [Option<C>; N],
    len: usize,
}

impl<C, const N: usize> Default for Transaction<C, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C, const N: usize> Transaction<C, N> {
    /// Create an empty transaction.
    pub const fn new() -> Self {
        Self {
            staged: [const { None }; N],
            len: 0,
        }
    }

    /// Maximum number of changes that can be staged.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Number of changes staged so far.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is staged.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the staging buffer is full.
    pub const fn is_full(&self) -> bool {
        self.len == N
    }

    /// Stage a change. On a full transaction the change is handed back via
    /// `Err` (nothing is partially staged).
    pub fn stage(&mut self, change: C) -> Result<(), C> {
        if self.len == N {
            return Err(change);
        }
        self.staged[self.len] = Some(change);
        self.len += 1;
        Ok(())
    }

    /// Iterate over the staged changes in stage order.
    pub fn changes(&self) -> impl Iterator<Item = &C> {
        self.staged[..self.len].iter().flatten()
    }

    /// Validate the whole batch without applying anything. Returns `true` only
    /// if every staged change passes `check`. This is the `TEST_ONLY` pass:
    /// side-effect-free by contract.
    pub fn validate(&self, check: impl FnMut(&C) -> bool) -> bool {
        self.changes().all(check)
    }

    /// Commit: apply every staged change in order, consuming the transaction.
    /// Call only after [`validate`] succeeds — the all-or-nothing guarantee is
    /// that the caller never applies a partially-valid batch.
    ///
    /// [`validate`]: Transaction::validate
    pub fn commit(mut self, mut apply: impl FnMut(C)) {
        for slot in self.staged[..self.len].iter_mut() {
            if let Some(change) = slot.take() {
                apply(change);
            }
        }
    }

    /// Abort: discard every staged change, applying nothing.
    pub fn abort(self) {
        // Dropping the transaction drops the staged changes. Spelled out so the
        // orchestrator's intent reads as the inverse of `commit`.
    }

    /// Discard staged changes but keep the (reusable) transaction.
    pub fn clear(&mut self) {
        for slot in self.staged[..self.len].iter_mut() {
            *slot = None;
        }
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_and_iterate_in_order() {
        let mut txn: Transaction<u32, 4> = Transaction::new();
        assert!(txn.is_empty());
        txn.stage(10).unwrap();
        txn.stage(20).unwrap();
        txn.stage(30).unwrap();
        assert_eq!(txn.len(), 3);
        let seen: Vec<u32> = txn.changes().copied().collect();
        assert_eq!(seen, vec![10, 20, 30]);
    }

    #[test]
    fn validate_is_all_or_nothing() {
        let mut txn: Transaction<i32, 4> = Transaction::new();
        txn.stage(1).unwrap();
        txn.stage(2).unwrap();
        txn.stage(3).unwrap();
        assert!(txn.validate(|&c| c > 0));
        assert!(!txn.validate(|&c| c > 1)); // change `1` fails
    }

    #[test]
    fn commit_applies_every_change_in_order() {
        let mut txn: Transaction<&str, 4> = Transaction::new();
        txn.stage("a").unwrap();
        txn.stage("b").unwrap();
        let mut applied: Vec<&str> = Vec::new();
        txn.commit(|c| applied.push(c));
        assert_eq!(applied, vec!["a", "b"]);
    }

    #[test]
    fn abort_applies_nothing() {
        let mut txn: Transaction<u8, 4> = Transaction::new();
        txn.stage(1).unwrap();
        txn.stage(2).unwrap();
        // The contract: if validation fails the orchestrator aborts and the
        // target is never touched. Here we simply never call commit.
        txn.abort();
        // (Nothing to assert on the target — that is the point: it is untouched.)
    }

    #[test]
    fn full_transaction_hands_the_change_back() {
        let mut txn: Transaction<u8, 2> = Transaction::new();
        txn.stage(1).unwrap();
        txn.stage(2).unwrap();
        assert!(txn.is_full());
        assert_eq!(txn.stage(3), Err(3));
    }

    #[test]
    fn clear_resets_for_reuse() {
        let mut txn: Transaction<u8, 2> = Transaction::new();
        txn.stage(1).unwrap();
        txn.clear();
        assert!(txn.is_empty());
        txn.stage(9).unwrap(); // reusable
        assert_eq!(txn.changes().copied().collect::<Vec<_>>(), vec![9]);
    }
}
