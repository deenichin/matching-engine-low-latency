//! Inbound commands the engine applies, and outbound events it emits.
//!
//! `wire` decodes directly into [`Command`] — there is no parallel wire type
//! hierarchy (SPEC §4). Matching logic that turns a `Command` into `Event`s
//! is built in stage 1; this module only defines the shapes.

use crate::error::RejectReason;
use crate::types::{AccountId, OrderId, OrderKind, Price, Qty, Side, Tif};

/// A decoded, gateway-validated instruction for the engine to apply.
///
/// Every variant that references a resting order carries both `account_id`
/// and `order_id`: lookups are keyed on the pair, never bare `OrderId`,
/// because `OrderId` is only unique per account (SPEC §2, §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    NewOrder {
        account_id: AccountId,
        order_id: OrderId,
        side: Side,
        /// Unused for `Market` orders — see SPEC §2 on why the field is
        /// still present on the wire.
        price: Price,
        qty: Qty,
        kind: OrderKind,
        tif: Tif,
        client_ts: u64,
    },
    CancelOrder {
        account_id: AccountId,
        order_id: OrderId,
    },
    CancelReplace {
        account_id: AccountId,
        order_id: OrderId,
        new_price: Price,
        new_qty: Qty,
    },
    MassCancel {
        account_id: AccountId,
    },
    KillSwitch {
        engaged: bool,
    },
    Snapshot,
}

/// Something the engine produced in response to a [`Command`].
///
/// A single dispatch may produce several `Event`s — e.g. a sweep across
/// levels emits one `Filled` per resting order it consumes, plus one for
/// the taker, plus possibly a `Cancelled` from self-trade prevention. Each
/// is sent individually, never collected into a `Vec`, per the channel
/// design in SPEC §4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The taker side of a submit that did not fill in full: whatever
    /// remains rests at `resting_qty` (zero for an IOC or Market order that
    /// discarded the remainder instead of resting it).
    Accepted {
        account_id: AccountId,
        order_id: OrderId,
        resting_qty: Qty,
    },
    Rejected {
        account_id: AccountId,
        order_id: OrderId,
        reason: RejectReason,
    },
    /// One side of a single match. A sweep or a crossing modify emits one
    /// of these per resting order it consumes, plus one for the taker.
    Filled {
        account_id: AccountId,
        order_id: OrderId,
        side: Side,
        /// Execution price — always the maker's resting price (SPEC §2),
        /// never the taker's limit.
        price: Price,
        qty: Qty,
        /// Quantity still resting on this order after this fill.
        resting_qty: Qty,
    },
    Cancelled {
        account_id: AccountId,
        order_id: OrderId,
    },
    /// Emitted before any `Filled` events a crossing modify produces
    /// (SPEC §2).
    Replaced {
        account_id: AccountId,
        order_id: OrderId,
        new_qty: Qty,
        priority_retained: bool,
    },
    /// Public market-data print. Carries no account identity.
    Trade {
        price: Price,
        qty: Qty,
        taker_side: Side,
    },
    /// Top-of-book snapshot. `None` on a side means that side is empty.
    BookUpdate {
        best_bid: Option<(Price, Qty)>,
        best_ask: Option<(Price, Qty)>,
    },
}
