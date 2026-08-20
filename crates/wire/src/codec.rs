//! Byte-level little-endian read/write primitives, and the `u8` mappings
//! for the small enums that appear in message bodies. Nothing here
//! allocates.

use core::error::RejectReason;
use core::types::{OrderKind, Side, Tif};

pub(crate) fn write_u8(buf: &mut [u8], offset: usize, value: u8) {
    buf[offset] = value;
}

pub(crate) fn read_u8(buf: &[u8], offset: usize) -> u8 {
    buf[offset]
}

pub(crate) fn write_u64(buf: &mut [u8], offset: usize, value: u64) {
    buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn read_u64(buf: &[u8], offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[offset..offset + 8]);
    u64::from_le_bytes(bytes)
}

pub(crate) fn write_bool(buf: &mut [u8], offset: usize, value: bool) {
    buf[offset] = value as u8;
}

pub(crate) fn read_bool(buf: &[u8], offset: usize) -> Result<bool, RejectReason> {
    match buf[offset] {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(RejectReason::MalformedMessage),
    }
}

pub(crate) fn side_to_u8(side: Side) -> u8 {
    match side {
        Side::Buy => 0,
        Side::Sell => 1,
    }
}

pub(crate) fn side_from_u8(value: u8) -> Result<Side, RejectReason> {
    match value {
        0 => Ok(Side::Buy),
        1 => Ok(Side::Sell),
        _ => Err(RejectReason::MalformedMessage),
    }
}

pub(crate) fn order_kind_to_u8(kind: OrderKind) -> u8 {
    match kind {
        OrderKind::Limit => 0,
        OrderKind::Market => 1,
    }
}

pub(crate) fn order_kind_from_u8(value: u8) -> Result<OrderKind, RejectReason> {
    match value {
        0 => Ok(OrderKind::Limit),
        1 => Ok(OrderKind::Market),
        _ => Err(RejectReason::MalformedMessage),
    }
}

pub(crate) fn tif_to_u8(tif: Tif) -> u8 {
    match tif {
        Tif::Gtc => 0,
        Tif::Ioc => 1,
        Tif::Fok => 2,
        Tif::PostOnly => 3,
    }
}

pub(crate) fn tif_from_u8(value: u8) -> Result<Tif, RejectReason> {
    match value {
        0 => Ok(Tif::Gtc),
        1 => Ok(Tif::Ioc),
        2 => Ok(Tif::Fok),
        3 => Ok(Tif::PostOnly),
        _ => Err(RejectReason::MalformedMessage),
    }
}

/// `RejectReason` already carries the exhaustive, wire-stable taxonomy
/// with explicit `#[repr(u8)]` discriminants (SPEC §5) — encode/decode
/// reuse those values directly rather than inventing a second mapping.
pub(crate) fn reject_reason_to_u8(reason: RejectReason) -> u8 {
    reason as u8
}

pub(crate) fn reject_reason_from_u8(value: u8) -> Result<RejectReason, RejectReason> {
    match value {
        0 => Ok(RejectReason::MalformedMessage),
        1 => Ok(RejectReason::UnknownMessageType),
        2 => Ok(RejectReason::DuplicateOrderId),
        3 => Ok(RejectReason::UnknownOrderId),
        4 => Ok(RejectReason::ZeroQuantity),
        5 => Ok(RejectReason::WouldCross),
        6 => Ok(RejectReason::NotFullyFillable),
        7 => Ok(RejectReason::KillSwitchActive),
        8 => Ok(RejectReason::MaxOpenOrders),
        9 => Ok(RejectReason::MaxNotional),
        10 => Ok(RejectReason::PriceBandViolation),
        11 => Ok(RejectReason::ZeroPrice),
        _ => Err(RejectReason::MalformedMessage),
    }
}
