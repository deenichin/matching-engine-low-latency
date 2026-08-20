//! Encode/decode for the six inbound message types, straight into and out
//! of `core::Command` — no parallel type hierarchy (SPEC §4).
//!
//! `decode_command` also performs the gateway-level validation SPEC §3
//! describes (frame length vs. tag, enum fields in range, quantity
//! nonzero, price nonzero except for `Market`): malformed input rejects
//! with a specific `RejectReason` here and never reaches `core`.

use core::error::RejectReason;
use core::event::Command;
use core::types::{AccountId, OrderId, OrderKind, Price, Qty};

use crate::codec::*;
use crate::tag::*;

/// Encode `cmd` into `buf`, returning the number of bytes written. `buf`
/// must be at least [`crate::tag::MAX_MESSAGE_LEN`] long.
///
/// # Panics
/// Panics if `buf` is too short for `cmd`'s encoded length. That's a
/// caller-owned-buffer sizing bug, not wire input — never triggered by
/// anything a peer sends us.
pub fn encode_command(cmd: &Command, buf: &mut [u8]) -> usize {
    match *cmd {
        Command::NewOrder {
            account_id,
            order_id,
            side,
            price,
            qty,
            kind,
            tif,
            client_ts,
        } => {
            write_u8(buf, 0, TAG_NEW_ORDER);
            write_u64(buf, 1, order_id.0);
            write_u64(buf, 9, account_id.0);
            write_u8(buf, 17, side_to_u8(side));
            write_u64(buf, 18, price.0);
            write_u64(buf, 26, qty.0);
            write_u8(buf, 34, order_kind_to_u8(kind));
            write_u8(buf, 35, tif_to_u8(tif));
            write_u64(buf, 36, client_ts);
            NEW_ORDER_LEN
        }
        Command::CancelOrder {
            account_id,
            order_id,
        } => {
            write_u8(buf, 0, TAG_CANCEL_ORDER);
            write_u64(buf, 1, order_id.0);
            write_u64(buf, 9, account_id.0);
            CANCEL_ORDER_LEN
        }
        Command::CancelReplace {
            account_id,
            order_id,
            new_price,
            new_qty,
        } => {
            write_u8(buf, 0, TAG_CANCEL_REPLACE);
            write_u64(buf, 1, order_id.0);
            write_u64(buf, 9, account_id.0);
            write_u64(buf, 17, new_price.0);
            write_u64(buf, 25, new_qty.0);
            CANCEL_REPLACE_LEN
        }
        Command::MassCancel { account_id } => {
            write_u8(buf, 0, TAG_MASS_CANCEL);
            write_u64(buf, 1, account_id.0);
            MASS_CANCEL_LEN
        }
        Command::KillSwitch { engaged } => {
            write_u8(buf, 0, TAG_KILL_SWITCH);
            write_bool(buf, 1, engaged);
            KILL_SWITCH_LEN
        }
        Command::Snapshot => {
            write_u8(buf, 0, TAG_SNAPSHOT);
            SNAPSHOT_LEN
        }
    }
}

/// Decode one frame's bytes into a `Command`, validating as SPEC §3
/// describes. `frame` should be exactly the tag's implied length — the
/// framer guarantees this in the real pipeline, but this function checks
/// it itself rather than trusting the caller, since a truncated frame is
/// exactly the kind of wire input this validation exists to catch.
pub fn decode_command(frame: &[u8]) -> Result<Command, RejectReason> {
    let Some(&tag) = frame.first() else {
        return Err(RejectReason::MalformedMessage);
    };
    let Some(expected_len) = message_len(tag) else {
        return Err(RejectReason::UnknownMessageType);
    };
    if frame.len() != expected_len {
        return Err(RejectReason::MalformedMessage);
    }

    match tag {
        TAG_NEW_ORDER => {
            let kind = order_kind_from_u8(read_u8(frame, 34))?;
            let price = Price(read_u64(frame, 18));
            let qty = Qty(read_u64(frame, 26));
            if qty.0 == 0 {
                return Err(RejectReason::ZeroQuantity);
            }
            // Price is unused for Market orders (SPEC §2) -- the gateway
            // exempts them from the nonzero-price rule.
            if kind == OrderKind::Limit && price.0 == 0 {
                return Err(RejectReason::ZeroPrice);
            }
            Ok(Command::NewOrder {
                order_id: OrderId(read_u64(frame, 1)),
                account_id: AccountId(read_u64(frame, 9)),
                side: side_from_u8(read_u8(frame, 17))?,
                price,
                qty,
                kind,
                tif: tif_from_u8(read_u8(frame, 35))?,
                client_ts: read_u64(frame, 36),
            })
        }
        TAG_CANCEL_ORDER => Ok(Command::CancelOrder {
            order_id: OrderId(read_u64(frame, 1)),
            account_id: AccountId(read_u64(frame, 9)),
        }),
        TAG_CANCEL_REPLACE => {
            let new_qty = Qty(read_u64(frame, 25));
            if new_qty.0 == 0 {
                return Err(RejectReason::ZeroQuantity);
            }
            Ok(Command::CancelReplace {
                order_id: OrderId(read_u64(frame, 1)),
                account_id: AccountId(read_u64(frame, 9)),
                new_price: Price(read_u64(frame, 17)),
                new_qty,
            })
        }
        TAG_MASS_CANCEL => Ok(Command::MassCancel {
            account_id: AccountId(read_u64(frame, 1)),
        }),
        TAG_KILL_SWITCH => Ok(Command::KillSwitch {
            engaged: read_bool(frame, 1)?,
        }),
        TAG_SNAPSHOT => Ok(Command::Snapshot),
        _ => unreachable!("message_len already rejected any tag not handled above"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::types::{Side, Tif};

    fn roundtrip(cmd: Command) {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let len = encode_command(&cmd, &mut buf);
        let decoded = decode_command(&buf[..len]).expect("encoded command must decode");
        assert_eq!(decoded, cmd);
    }

    #[test]
    fn new_order_round_trips() {
        roundtrip(Command::NewOrder {
            account_id: AccountId(1),
            order_id: OrderId(2),
            side: Side::Buy,
            price: Price(100),
            qty: Qty(5),
            kind: OrderKind::Limit,
            tif: Tif::Gtc,
            client_ts: 123_456,
        });
    }

    #[test]
    fn new_order_market_round_trips_with_unused_price() {
        // Market's price is unused (SPEC §2) -- it can be anything on the
        // wire, including 0, and still round-trip.
        roundtrip(Command::NewOrder {
            account_id: AccountId(1),
            order_id: OrderId(2),
            side: Side::Sell,
            price: Price(0),
            qty: Qty(5),
            kind: OrderKind::Market,
            tif: Tif::Ioc,
            client_ts: 0,
        });
    }

    #[test]
    fn cancel_order_round_trips() {
        roundtrip(Command::CancelOrder {
            account_id: AccountId(7),
            order_id: OrderId(9),
        });
    }

    #[test]
    fn cancel_replace_round_trips() {
        roundtrip(Command::CancelReplace {
            account_id: AccountId(7),
            order_id: OrderId(9),
            new_price: Price(150),
            new_qty: Qty(3),
        });
    }

    #[test]
    fn mass_cancel_round_trips() {
        roundtrip(Command::MassCancel {
            account_id: AccountId(42),
        });
    }

    #[test]
    fn kill_switch_round_trips_both_states() {
        roundtrip(Command::KillSwitch { engaged: true });
        roundtrip(Command::KillSwitch { engaged: false });
    }

    #[test]
    fn snapshot_round_trips() {
        roundtrip(Command::Snapshot);
    }

    #[test]
    fn truncated_frame_is_rejected() {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let len = encode_command(
            &Command::CancelOrder {
                account_id: AccountId(1),
                order_id: OrderId(2),
            },
            &mut buf,
        );
        // One byte short of CancelOrder's full 17.
        let result = decode_command(&buf[..len - 1]);
        assert_eq!(result, Err(RejectReason::MalformedMessage));
    }

    #[test]
    fn empty_frame_is_rejected() {
        assert_eq!(decode_command(&[]), Err(RejectReason::MalformedMessage));
    }

    #[test]
    fn unknown_tag_is_rejected() {
        let frame = [255u8; 20];
        assert_eq!(
            decode_command(&frame),
            Err(RejectReason::UnknownMessageType)
        );
    }

    #[test]
    fn out_of_range_side_is_rejected() {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        encode_command(
            &Command::NewOrder {
                account_id: AccountId(1),
                order_id: OrderId(2),
                side: Side::Buy,
                price: Price(100),
                qty: Qty(5),
                kind: OrderKind::Limit,
                tif: Tif::Gtc,
                client_ts: 0,
            },
            &mut buf,
        );
        buf[17] = 200; // side byte, out of range (only 0/1 are valid)
        assert_eq!(
            decode_command(&buf[..NEW_ORDER_LEN]),
            Err(RejectReason::MalformedMessage)
        );
    }

    #[test]
    fn out_of_range_tif_is_rejected() {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        encode_command(
            &Command::NewOrder {
                account_id: AccountId(1),
                order_id: OrderId(2),
                side: Side::Buy,
                price: Price(100),
                qty: Qty(5),
                kind: OrderKind::Limit,
                tif: Tif::Gtc,
                client_ts: 0,
            },
            &mut buf,
        );
        buf[35] = 200; // tif byte, out of range (only 0..=3 are valid)
        assert_eq!(
            decode_command(&buf[..NEW_ORDER_LEN]),
            Err(RejectReason::MalformedMessage)
        );
    }

    #[test]
    fn zero_qty_new_order_is_rejected() {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        encode_command(
            &Command::NewOrder {
                account_id: AccountId(1),
                order_id: OrderId(2),
                side: Side::Buy,
                price: Price(100),
                qty: Qty(0),
                kind: OrderKind::Limit,
                tif: Tif::Gtc,
                client_ts: 0,
            },
            &mut buf,
        );
        assert_eq!(
            decode_command(&buf[..NEW_ORDER_LEN]),
            Err(RejectReason::ZeroQuantity)
        );
    }

    #[test]
    fn zero_qty_cancel_replace_is_rejected() {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        encode_command(
            &Command::CancelReplace {
                account_id: AccountId(1),
                order_id: OrderId(2),
                new_price: Price(100),
                new_qty: Qty(0),
            },
            &mut buf,
        );
        assert_eq!(
            decode_command(&buf[..CANCEL_REPLACE_LEN]),
            Err(RejectReason::ZeroQuantity)
        );
    }

    #[test]
    fn zero_price_limit_new_order_is_rejected() {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        encode_command(
            &Command::NewOrder {
                account_id: AccountId(1),
                order_id: OrderId(2),
                side: Side::Buy,
                price: Price(0),
                qty: Qty(5),
                kind: OrderKind::Limit,
                tif: Tif::Gtc,
                client_ts: 0,
            },
            &mut buf,
        );
        assert_eq!(
            decode_command(&buf[..NEW_ORDER_LEN]),
            Err(RejectReason::ZeroPrice)
        );
    }
}
