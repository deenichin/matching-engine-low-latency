//! The exhaustive reject-reason taxonomy (SPEC §5). Each variant carries a
//! distinct wire value so the wire encoding built in stage 2 is a direct
//! mapping, not a translation layer.

/// Why a command was rejected. Exhaustive — every reject in the system uses
/// one of these, never a generic or opaque failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RejectReason {
    MalformedMessage = 0,
    UnknownMessageType = 1,
    DuplicateOrderId = 2,
    UnknownOrderId = 3,
    ZeroQuantity = 4,
    WouldCross = 5,
    NotFullyFillable = 6,
    KillSwitchActive = 7,
    MaxOpenOrders = 8,
    MaxNotional = 9,
    PriceBandViolation = 10,
}
