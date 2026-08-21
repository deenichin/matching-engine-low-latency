//! Domain types. Unit semantics for [`Price`] and [`Qty`] are documented on
//! the types themselves — see SPEC.md §2 for the full rationale.

/// Price in integer ticks of quote currency per base-currency lot.
///
/// One tick is fixed at **$0.01** (see [`TICK_SIZE_CENTS`]) — never a float.
/// `Price` and [`Qty`] are integers specifically so that `price * qty` (the
/// notional arithmetic in SPEC §5) is exact, with no rounding error to
/// reason about. For a BTC/USDT book: `Price` is USDT-per-BTC in cents,
/// `Qty` is BTC in ten-thousandths, and their product is USDT cents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(pub u64);

/// Quantity in integer lots of base currency.
///
/// One lot is fixed at **0.0001 base units** (see [`LOT_SIZE`]) — never a
/// float. Paired with [`Price`], `price * qty` is denominated in
/// quote-currency ticks, which is exactly the quantity the gross notional
/// check in SPEC §5 sums and caps — the scale of a lot is not a
/// display-only detail, it is what makes that arithmetic mean what it says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Qty(pub u64);

/// Client-assigned order identifier.
///
/// Unique only among an account's *currently active* orders — **not**
/// globally unique. Two different accounts may legally submit the same
/// numeric id concurrently. Every lookup, index, and duplicate check is
/// therefore keyed on `(AccountId, OrderId)`, never bare `OrderId`
/// (SPEC §2, §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OrderId(pub u64);

/// Account identifier. Every order carries one; it is never optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AccountId(pub u64);

/// Engine-assigned sequence number, strictly monotonic across every command
/// the engine applies.
///
/// Used for internal ordering and the determinism comparison in SPEC §7.
/// This is **not** a per-stream sequence number and never appears on an
/// outbound message directly — see [`StreamSeq`] for that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EngineSeq(pub u64);

/// Per-outbound-stream sequence number.
///
/// One independent monotonic counter per stream — execution reports and
/// market data each have their own (SPEC §2, §8). Gap detection is only
/// meaningful against a counter that increments exactly once per message
/// actually delivered on *that* stream; a counter shared across streams
/// would produce a permanent phantom gap on one stream for every event that
/// went out the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamSeq(pub u64);

/// Which side of the book an order or fill is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Buy,
    Sell,
}

/// Whether an order is a resting-capable limit order or an immediate-only
/// market order.
///
/// `Market` has its own semantics, distinct from any [`Tif`] value — see
/// SPEC §2's order type table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderKind {
    Limit,
    Market,
}

/// Time-in-force for an [`OrderKind::Limit`] order.
///
/// Not meaningful for `Market`, which always behaves as IOC with an
/// unbounded limit regardless of any `Tif` carried on the wire (SPEC §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tif {
    /// Match what it can, rest the remainder.
    Gtc,
    /// Match what it can, discard the remainder.
    Ioc,
    /// Match in full immediately, or reject with zero fills.
    Fok,
    /// Reject if it would cross after self-trade prevention; otherwise rest
    /// in full.
    PostOnly,
}

/// A fill's order-status distinction: whether the order it belongs to has
/// any quantity left resting after this fill, or has been fully consumed.
///
/// Mirrors FIX's `ExecutionReport`/`OrdStatus` (tag 39) design — a status
/// *field* on one execution message type, not a second message type. This
/// protocol already collapses FIX's `ExecType` (tag 150) into the message
/// tag itself (one tag per `core::Event` variant, ITCH/OUCH-style, SPEC
/// §3); `state` recovers the `OrdStatus` distinction without reopening
/// that collapse into a `PartiallyFilled` tag alongside `Filled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FillState {
    PartiallyFilled,
    Filled,
}

/// Cents per price tick. `Price` is denominated in ticks; one tick is
/// exactly one cent (SPEC §2).
pub const TICK_SIZE_CENTS: u64 = 1;

/// Lots per one whole base-currency unit. `Qty` is denominated in lots; one
/// lot is `1 / LOT_SIZE` of a base unit, i.e. `0.0001` (SPEC §2).
pub const LOT_SIZE: u64 = 10_000;

/// The single hardcoded instrument this engine trades (SPEC §2: "Symbol is
/// hardcoded to one instrument").
pub const SYMBOL: &str = "BTC-USDT";

/// Maximum resting orders per account (SPEC §5).
pub const MAX_OPEN_ORDERS: usize = 50;

/// Maximum gross notional per account, in price ticks. `100_000_000` ticks
/// is `$1,000,000` at `TICK_SIZE_CENTS = 1` (SPEC §5).
pub const MAX_NOTIONAL_TICKS: u128 = 100_000_000;

/// Price band half-width, as a percentage of the reference price
/// (SPEC §5: ±10%).
pub const PRICE_BAND_PCT: u64 = 10;
