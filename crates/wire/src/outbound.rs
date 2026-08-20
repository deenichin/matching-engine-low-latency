//! Encode/decode for the seven outbound event types, straight into and out
//! of `core::Event` — no parallel type hierarchy (SPEC §4).
//!
//! `core::Event` itself carries no sequence number: `core` doesn't know
//! about outbound streams (SPEC §2's `EngineSeq`/`StreamSeq` split exists
//! precisely so a single command's internal ordering doesn't get confused
//! with per-stream sequencing). The caller — whoever owns the stream this
//! event is about to go out on, gateway for execution reports or
//! `marketdata` for market data — supplies the `StreamSeq` to stamp.

use core::error::RejectReason;
use core::event::Event;
use core::types::{AccountId, OrderId, Price, Qty, StreamSeq};

use crate::codec::*;
use crate::tag::*;

/// Encode `event`, stamped with `seq`, into `buf`. Returns the number of
/// bytes written. `buf` must be at least [`crate::tag::MAX_MESSAGE_LEN`]
/// long.
///
/// # Panics
/// Panics if `buf` is too short — a caller-owned-buffer sizing bug, never
/// triggered by wire input (this function never reads the network).
pub fn encode_event(seq: StreamSeq, event: &Event, buf: &mut [u8]) -> usize {
    match *event {
        Event::Accepted {
            account_id,
            order_id,
            resting_qty,
        } => {
            write_u8(buf, 0, TAG_ACCEPTED);
            write_u64(buf, 1, seq.0);
            write_u64(buf, 9, account_id.0);
            write_u64(buf, 17, order_id.0);
            write_u64(buf, 25, resting_qty.0);
            ACCEPTED_LEN
        }
        Event::Rejected {
            account_id,
            order_id,
            reason,
        } => {
            write_u8(buf, 0, TAG_REJECTED);
            write_u64(buf, 1, seq.0);
            write_u64(buf, 9, account_id.0);
            write_u64(buf, 17, order_id.0);
            write_u8(buf, 25, reject_reason_to_u8(reason));
            REJECTED_LEN
        }
        Event::Filled {
            account_id,
            order_id,
            side,
            price,
            qty,
            resting_qty,
        } => {
            write_u8(buf, 0, TAG_FILLED);
            write_u64(buf, 1, seq.0);
            write_u64(buf, 9, account_id.0);
            write_u64(buf, 17, order_id.0);
            write_u8(buf, 25, side_to_u8(side));
            write_u64(buf, 26, price.0);
            write_u64(buf, 34, qty.0);
            write_u64(buf, 42, resting_qty.0);
            FILLED_LEN
        }
        Event::Cancelled {
            account_id,
            order_id,
        } => {
            write_u8(buf, 0, TAG_CANCELLED);
            write_u64(buf, 1, seq.0);
            write_u64(buf, 9, account_id.0);
            write_u64(buf, 17, order_id.0);
            CANCELLED_LEN
        }
        Event::Replaced {
            account_id,
            order_id,
            new_qty,
            priority_retained,
        } => {
            write_u8(buf, 0, TAG_REPLACED);
            write_u64(buf, 1, seq.0);
            write_u64(buf, 9, account_id.0);
            write_u64(buf, 17, order_id.0);
            write_u64(buf, 25, new_qty.0);
            write_bool(buf, 33, priority_retained);
            REPLACED_LEN
        }
        Event::Trade {
            price,
            qty,
            taker_side,
        } => {
            write_u8(buf, 0, TAG_TRADE);
            write_u64(buf, 1, seq.0);
            write_u64(buf, 9, price.0);
            write_u64(buf, 17, qty.0);
            write_u8(buf, 25, side_to_u8(taker_side));
            TRADE_LEN
        }
        Event::BookUpdate { best_bid, best_ask } => {
            write_u8(buf, 0, TAG_BOOK_UPDATE);
            write_u64(buf, 1, seq.0);
            write_bool(buf, 9, best_bid.is_some());
            let (bid_price, bid_qty) = best_bid.unwrap_or((Price(0), Qty(0)));
            write_u64(buf, 10, bid_price.0);
            write_u64(buf, 18, bid_qty.0);
            write_bool(buf, 26, best_ask.is_some());
            let (ask_price, ask_qty) = best_ask.unwrap_or((Price(0), Qty(0)));
            write_u64(buf, 27, ask_price.0);
            write_u64(buf, 35, ask_qty.0);
            BOOK_UPDATE_LEN
        }
    }
}

/// Decode one frame's bytes into `(StreamSeq, Event)`. `frame` should be
/// exactly the tag's implied length.
pub fn decode_event(frame: &[u8]) -> Result<(StreamSeq, Event), RejectReason> {
    let Some(&tag) = frame.first() else {
        return Err(RejectReason::MalformedMessage);
    };
    let Some(expected_len) = message_len(tag) else {
        return Err(RejectReason::UnknownMessageType);
    };
    if frame.len() != expected_len {
        return Err(RejectReason::MalformedMessage);
    }
    let seq = StreamSeq(read_u64(frame, 1));

    let event = match tag {
        TAG_ACCEPTED => Event::Accepted {
            account_id: AccountId(read_u64(frame, 9)),
            order_id: OrderId(read_u64(frame, 17)),
            resting_qty: Qty(read_u64(frame, 25)),
        },
        TAG_REJECTED => Event::Rejected {
            account_id: AccountId(read_u64(frame, 9)),
            order_id: OrderId(read_u64(frame, 17)),
            reason: reject_reason_from_u8(read_u8(frame, 25))?,
        },
        TAG_FILLED => Event::Filled {
            account_id: AccountId(read_u64(frame, 9)),
            order_id: OrderId(read_u64(frame, 17)),
            side: side_from_u8(read_u8(frame, 25))?,
            price: Price(read_u64(frame, 26)),
            qty: Qty(read_u64(frame, 34)),
            resting_qty: Qty(read_u64(frame, 42)),
        },
        TAG_CANCELLED => Event::Cancelled {
            account_id: AccountId(read_u64(frame, 9)),
            order_id: OrderId(read_u64(frame, 17)),
        },
        TAG_REPLACED => Event::Replaced {
            account_id: AccountId(read_u64(frame, 9)),
            order_id: OrderId(read_u64(frame, 17)),
            new_qty: Qty(read_u64(frame, 25)),
            priority_retained: read_bool(frame, 33)?,
        },
        TAG_TRADE => Event::Trade {
            price: Price(read_u64(frame, 9)),
            qty: Qty(read_u64(frame, 17)),
            taker_side: side_from_u8(read_u8(frame, 25))?,
        },
        TAG_BOOK_UPDATE => Event::BookUpdate {
            best_bid: read_bool(frame, 9)?
                .then(|| (Price(read_u64(frame, 10)), Qty(read_u64(frame, 18)))),
            best_ask: read_bool(frame, 26)?
                .then(|| (Price(read_u64(frame, 27)), Qty(read_u64(frame, 35)))),
        },
        _ => unreachable!("message_len already rejected any tag not handled above"),
    };
    Ok((seq, event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::types::Side;

    fn roundtrip(seq: u64, event: Event) {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let len = encode_event(StreamSeq(seq), &event, &mut buf);
        let (decoded_seq, decoded_event) =
            decode_event(&buf[..len]).expect("encoded event must decode");
        assert_eq!(decoded_seq, StreamSeq(seq));
        assert_eq!(decoded_event, event);
    }

    #[test]
    fn accepted_round_trips() {
        roundtrip(
            1,
            Event::Accepted {
                account_id: AccountId(1),
                order_id: OrderId(2),
                resting_qty: Qty(5),
            },
        );
    }

    #[test]
    fn rejected_round_trips() {
        roundtrip(
            2,
            Event::Rejected {
                account_id: AccountId(1),
                order_id: OrderId(2),
                reason: RejectReason::PriceBandViolation,
            },
        );
        roundtrip(
            2,
            Event::Rejected {
                account_id: AccountId(1),
                order_id: OrderId(2),
                reason: RejectReason::ZeroPrice,
            },
        );
    }

    #[test]
    fn filled_round_trips() {
        roundtrip(
            3,
            Event::Filled {
                account_id: AccountId(1),
                order_id: OrderId(2),
                side: Side::Sell,
                price: Price(100),
                qty: Qty(4),
                resting_qty: Qty(1),
            },
        );
    }

    #[test]
    fn cancelled_round_trips() {
        roundtrip(
            4,
            Event::Cancelled {
                account_id: AccountId(1),
                order_id: OrderId(2),
            },
        );
    }

    #[test]
    fn replaced_round_trips_both_priority_states() {
        roundtrip(
            5,
            Event::Replaced {
                account_id: AccountId(1),
                order_id: OrderId(2),
                new_qty: Qty(3),
                priority_retained: true,
            },
        );
        roundtrip(
            6,
            Event::Replaced {
                account_id: AccountId(1),
                order_id: OrderId(2),
                new_qty: Qty(3),
                priority_retained: false,
            },
        );
    }

    #[test]
    fn trade_round_trips() {
        roundtrip(
            7,
            Event::Trade {
                price: Price(100),
                qty: Qty(4),
                taker_side: Side::Buy,
            },
        );
    }

    #[test]
    fn book_update_round_trips_both_sides_present() {
        roundtrip(
            8,
            Event::BookUpdate {
                best_bid: Some((Price(99), Qty(10))),
                best_ask: Some((Price(101), Qty(7))),
            },
        );
    }

    #[test]
    fn book_update_round_trips_empty_sides() {
        roundtrip(
            9,
            Event::BookUpdate {
                best_bid: None,
                best_ask: None,
            },
        );
    }

    #[test]
    fn truncated_frame_is_rejected() {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let len = encode_event(
            StreamSeq(1),
            &Event::Cancelled {
                account_id: AccountId(1),
                order_id: OrderId(2),
            },
            &mut buf,
        );
        let result = decode_event(&buf[..len - 1]);
        assert_eq!(result, Err(RejectReason::MalformedMessage));
    }

    #[test]
    fn unknown_tag_is_rejected() {
        let frame = [255u8; 20];
        assert_eq!(decode_event(&frame), Err(RejectReason::UnknownMessageType));
    }

    #[test]
    fn out_of_range_reason_is_rejected() {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        encode_event(
            StreamSeq(1),
            &Event::Rejected {
                account_id: AccountId(1),
                order_id: OrderId(2),
                reason: RejectReason::ZeroQuantity,
            },
            &mut buf,
        );
        buf[25] = 200; // reason byte, out of range
        assert_eq!(
            decode_event(&buf[..REJECTED_LEN]),
            Err(RejectReason::MalformedMessage)
        );
    }
}
