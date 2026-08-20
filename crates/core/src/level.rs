//! One price level. Chain maintenance (append at tail, unlink from
//! anywhere) lives in `Book::rest`/`Book::unlink` (`book.rs`), not here —
//! this type only holds the shape.

use crate::arena::NULL;
use crate::types::Qty;

/// One price level: an intrusive doubly-linked FIFO of resting orders,
/// threaded through the arena, plus cached `total_qty` and `count` so depth
/// queries and the FOK precheck don't have to walk the chain (SPEC §4).
#[derive(Debug, Clone, Copy)]
pub struct Level {
    /// Arena slot of the first (oldest) order, or [`NULL`] if empty.
    pub(crate) head: u32,
    /// Arena slot of the last (newest) order, or [`NULL`] if empty.
    pub(crate) tail: u32,
    pub(crate) total_qty: Qty,
    pub(crate) count: u32,
}

impl Default for Level {
    /// A level with no resting orders. Per SPEC §4 / CLAUDE.md, a level in
    /// this state must never remain as a key in the price map — it exists
    /// only transiently, mid-mutation.
    fn default() -> Self {
        Self {
            head: NULL,
            tail: NULL,
            total_qty: Qty(0),
            count: 0,
        }
    }
}

impl Level {
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn total_qty(&self) -> Qty {
        self.total_qty
    }

    pub fn count(&self) -> u32 {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_level_is_empty() {
        let level = Level::default();
        assert!(level.is_empty());
        assert_eq!(level.count(), 0);
        assert_eq!(level.total_qty(), Qty(0));
    }
}
