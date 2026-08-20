//! Per-account resting-order index. Maintenance of `slots`/`acct_idx` lives
//! in exactly two places — `Book::rest` and `Book::unlink` (`book.rs`) —
//! and nowhere else; any other path that unlinks a node desyncs this
//! silently.

use crate::types::MAX_OPEN_ORDERS;

/// Per-account state, answering two questions in O(1) without walking the
/// book: current open-order count and gross notional (needed on every
/// submit, for risk), and the set of an account's resting orders (needed
/// for `MassCancel`) (SPEC §4).
#[derive(Debug, Clone)]
pub struct AccountEntry {
    /// Arena slots for this account's resting orders. Reserved to
    /// [`MAX_OPEN_ORDERS`] when the account is first seen and never grows —
    /// risk rejects at the cap before it could, which makes this a
    /// one-time allocation per account, not a hot-path allocation.
    pub(crate) slots: Vec<u32>,
    /// Sum of `price * qty` across this account's resting orders,
    /// regardless of side (SPEC §5: notional is gross, not net).
    pub(crate) notional: u128,
}

impl Default for AccountEntry {
    fn default() -> Self {
        Self {
            slots: Vec::with_capacity(MAX_OPEN_ORDERS),
            notional: 0,
        }
    }
}

impl AccountEntry {
    /// Open-order count. Deliberately `slots.len()` rather than a separate
    /// cached field, so there is one fewer value that can drift out of
    /// sync (SPEC §4).
    pub fn open_order_count(&self) -> usize {
        self.slots.len()
    }

    pub fn notional(&self) -> u128 {
        self.notional
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_account_entry_is_empty_and_pre_reserved() {
        let entry = AccountEntry::default();
        assert_eq!(entry.open_order_count(), 0);
        assert_eq!(entry.notional(), 0);
        assert!(entry.slots.capacity() >= MAX_OPEN_ORDERS);
    }
}
