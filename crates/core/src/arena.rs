//! Slot-stable node storage. `insert`/`free_slot` are the raw arena
//! primitives; account- and level-index maintenance around them lives in
//! `Book::rest`/`Book::unlink` (`book.rs`), not here.

use crate::types::{AccountId, OrderId, Price, Qty, Side};

/// Sentinel arena-slot value meaning "no node" — used for the free list and
/// for the head/prev/tail/next boundary links of a [`crate::level::Level`]'s
/// chain. Plain `u32`, not `Option<u32>`: SPEC §4 is explicit that slot
/// indices staying valid until freed is what allows plain `u32` links
/// instead of `Rc<RefCell<_>>` — no per-node allocation, no runtime borrow
/// checks, no reference-cycle leak risk.
pub const NULL: u32 = u32::MAX;

/// A single resting order, stored in a slot-stable arena.
///
/// Intended to stay within one 64-byte cache line (SPEC §4) — matching
/// sweeps touch nodes sequentially, so node size dominates far more than
/// any single auxiliary structure. Adding a field here requires a
/// deliberate size check against that budget, not a silent grow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    pub id: OrderId,
    pub account: AccountId,
    pub price: Price,
    pub qty: Qty,
    pub side: Side,
    /// Previous node in this price level's FIFO chain, or [`NULL`] if this
    /// is the head.
    pub prev: u32,
    /// Next node in this price level's FIFO chain, or [`NULL`] if this is
    /// the tail.
    pub next: u32,
    /// This node's own position in its account's `AccountEntry::slots`
    /// (SPEC §4) — lets cancel and mass-cancel maintain that array in O(1)
    /// without a reverse scan to find it.
    pub acct_idx: u32,
}

/// Slot-stable node storage with a free list.
///
/// Slot indices stay valid until freed, which is what lets [`Node`] links
/// be plain `u32` instead of `Rc<RefCell<_>>` (SPEC §4). Reused over an
/// intrusive-chain-only design specifically because an index lookup alone
/// only locates a *level*; without a stable slot to link through, removing
/// a node from the middle of a level's chain would be O(n).
#[derive(Debug, Default)]
pub struct Arena {
    pub(crate) slots: Vec<Option<Node>>,
    pub(crate) free: Vec<u32>,
}

impl Arena {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }

    pub fn get(&self, slot: u32) -> Option<&Node> {
        self.slots.get(slot as usize).and_then(Option::as_ref)
    }

    pub fn get_mut(&mut self, slot: u32) -> Option<&mut Node> {
        self.slots.get_mut(slot as usize).and_then(Option::as_mut)
    }

    /// Insert `node` into a free slot, reusing one from the free list if
    /// available, and return the slot it landed in.
    pub(crate) fn insert(&mut self, node: Node) -> u32 {
        if let Some(slot) = self.free.pop() {
            self.slots[slot as usize] = Some(node);
            slot
        } else {
            let slot = self.slots.len() as u32;
            self.slots.push(Some(node));
            slot
        }
    }

    /// Free a slot, returning it to the free list.
    pub(crate) fn free_slot(&mut self, slot: u32) {
        // Internal invariant: callers only ever free a slot they just read
        // a live node out of (see `Book::unlink`), so it cannot already be
        // free.
        let existing = self.slots[slot as usize].take();
        debug_assert!(existing.is_some(), "freeing a slot that was already free");
        self.free.push(slot);
    }

    /// Number of live (non-free) slots. For invariant-checking and tests.
    pub fn len(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_fits_in_one_cache_line() {
        let size = std::mem::size_of::<Node>();
        println!("size_of::<Node>() = {size} bytes");
        assert!(
            size <= 64,
            "Node is {size} bytes, over the 64-byte cache-line budget (SPEC §4)"
        );
    }

    #[test]
    fn fresh_arena_is_empty() {
        let arena = Arena::new();
        assert!(arena.is_empty());
        assert_eq!(arena.len(), 0);
        assert!(arena.get(0).is_none());
    }
}
