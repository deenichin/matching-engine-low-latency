//! Encode/decode for the outbound event types, straight into and out of
//! `core::Event` — no parallel type hierarchy (SPEC §4).
//!
//! `core::Event` itself carries no sequence number: `core` doesn't know
//! about outbound streams (SPEC §2's `EngineSeq`/`StreamSeq` split exists
//! precisely so a single command's internal ordering doesn't get confused
//! with per-stream sequencing). The caller — whoever owns the stream this
//! event is about to go out on, gateway for execution reports or
//! `marketdata` for market data — supplies the `StreamSeq` to stamp, and,
//! for execution reports only, the `EngineSeq` that command was assigned
//! by `risk::process_command`.
//!
//! `engine_seq` appears on the five execution-report types
//! (`Accepted`/`Rejected`/`Filled`/`Cancelled`/`Replaced`) and nowhere
//! else: `Trade`/`BookUpdate` are market data, which has no notion of a
//! single command's engine-assigned sequence, and the three `Snapshot*`
//! types are a point-in-time dump, not tied to any one command either.
//!
//! `Rejected` has one further wrinkle: a malformed frame or unknown tag
//! is rejected by the gateway's reader *before* `wire::decode_command`
//! ever produces a `Command`, so it never reaches `risk::process_command`
//! and has no real `EngineSeq` to carry. `EngineSeq(0)` is reserved as
//! the sentinel for exactly that case (`EngineSeq` starts at `1` for the
//! first command that actually enters the system, since
//! `risk::process_command` increments before it reads) — it never
//! appears on any event that passed through risk (SPEC §2).

use core::error::RejectReason;
use core::event::Event;
use core::types::{AccountId, EngineSeq, OrderId, Price, Qty, StreamSeq};

use crate::codec::*;
use crate::tag::*;

/// Encode `event`, stamped with `seq` and (for execution reports)
/// `engine_seq`, into `buf`. Returns the number of bytes written. `buf`
/// must be at least [`crate::tag::MAX_MESSAGE_LEN`] long.
///
/// # Panics
/// Panics if `buf` is too short — a caller-owned-buffer sizing bug, never
/// triggered by wire input (this function never reads the network).
/// Also panics if `engine_seq` is `None` for one of the five execution-
/// report types — every caller that emits one of those (the gateway's
/// return dispatcher, `bin::replay_file`) always has a real `EngineSeq`
/// on hand by construction, since both go through
/// `risk::process_command`; `None` is only ever passed for market data
/// and `Snapshot*`, which don't reach these arms.
pub fn encode_event(
    seq: StreamSeq,
    engine_seq: Option<EngineSeq>,
    event: &Event,
    buf: &mut [u8],
) -> usize {
    match *event {
        Event::Accepted {
            account_id,
            order_id,
            resting_qty,
        } => {
            write_u8(buf, 0, TAG_ACCEPTED);
            write_u64(buf, 1, seq.0);
            write_u64(
                buf,
                9,
                engine_seq
                    .expect("Accepted is an execution report; engine_seq is always Some")
                    .0,
            );
            write_u64(buf, 17, account_id.0);
            write_u64(buf, 25, order_id.0);
            write_u64(buf, 33, resting_qty.0);
            ACCEPTED_LEN
        }
        Event::Rejected {
            account_id,
            order_id,
            reason,
        } => {
            write_u8(buf, 0, TAG_REJECTED);
            write_u64(buf, 1, seq.0);
            write_u64(
                buf,
                9,
                engine_seq
                    .expect(
                        "Rejected always carries an EngineSeq -- either a real value from \
                         risk::process_command, or the EngineSeq(0) sentinel for a wire-level \
                         reject that never became a Command (SPEC §2)",
                    )
                    .0,
            );
            write_u64(buf, 17, account_id.0);
            write_u64(buf, 25, order_id.0);
            write_u8(buf, 33, reject_reason_to_u8(reason));
            REJECTED_LEN
        }
        Event::Filled {
            account_id,
            order_id,
            side,
            price,
            qty,
            resting_qty,
            state,
        } => {
            write_u8(buf, 0, TAG_FILLED);
            write_u64(buf, 1, seq.0);
            write_u64(
                buf,
                9,
                engine_seq
                    .expect("Filled is an execution report; engine_seq is always Some")
                    .0,
            );
            write_u64(buf, 17, account_id.0);
            write_u64(buf, 25, order_id.0);
            write_u8(buf, 33, side_to_u8(side));
            write_u64(buf, 34, price.0);
            write_u64(buf, 42, qty.0);
            write_u64(buf, 50, resting_qty.0);
            write_u8(buf, 58, fill_state_to_u8(state));
            FILLED_LEN
        }
        Event::Cancelled {
            account_id,
            order_id,
        } => {
            write_u8(buf, 0, TAG_CANCELLED);
            write_u64(buf, 1, seq.0);
            write_u64(
                buf,
                9,
                engine_seq
                    .expect("Cancelled is an execution report; engine_seq is always Some")
                    .0,
            );
            write_u64(buf, 17, account_id.0);
            write_u64(buf, 25, order_id.0);
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
            write_u64(
                buf,
                9,
                engine_seq
                    .expect("Replaced is an execution report; engine_seq is always Some")
                    .0,
            );
            write_u64(buf, 17, account_id.0);
            write_u64(buf, 25, order_id.0);
            write_u64(buf, 33, new_qty.0);
            write_bool(buf, 41, priority_retained);
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
        Event::SnapshotLevel {
            side,
            price,
            qty,
            order_count,
        } => {
            write_u8(buf, 0, TAG_SNAPSHOT_LEVEL);
            write_u64(buf, 1, seq.0);
            write_u8(buf, 9, side_to_u8(side));
            write_u64(buf, 10, price.0);
            write_u64(buf, 18, qty.0);
            write_u64(buf, 26, order_count);
            SNAPSHOT_LEVEL_LEN
        }
        Event::SnapshotAccount {
            account_id,
            open_order_count,
            notional,
        } => {
            write_u8(buf, 0, TAG_SNAPSHOT_ACCOUNT);
            write_u64(buf, 1, seq.0);
            write_u64(buf, 9, account_id.0);
            write_u64(buf, 17, open_order_count);
            write_u128(buf, 25, notional);
            SNAPSHOT_ACCOUNT_LEN
        }
        Event::SnapshotSummary {
            best_bid,
            best_ask,
            last_trade,
            level_count,
            account_count,
        } => {
            write_u8(buf, 0, TAG_SNAPSHOT_SUMMARY);
            write_u64(buf, 1, seq.0);
            write_bool(buf, 9, best_bid.is_some());
            let (bid_price, bid_qty) = best_bid.unwrap_or((Price(0), Qty(0)));
            write_u64(buf, 10, bid_price.0);
            write_u64(buf, 18, bid_qty.0);
            write_bool(buf, 26, best_ask.is_some());
            let (ask_price, ask_qty) = best_ask.unwrap_or((Price(0), Qty(0)));
            write_u64(buf, 27, ask_price.0);
            write_u64(buf, 35, ask_qty.0);
            write_bool(buf, 43, last_trade.is_some());
            write_u64(buf, 44, last_trade.unwrap_or(Price(0)).0);
            write_u64(buf, 52, level_count);
            write_u64(buf, 60, account_count);
            SNAPSHOT_SUMMARY_LEN
        }
    }
}

/// Decode one frame's bytes into `(StreamSeq, Option<EngineSeq>, Event)`.
/// `frame` should be exactly the tag's implied length. `EngineSeq` is
/// `Some` only for the five execution-report tags — see this module's
/// doc comment.
pub fn decode_event(frame: &[u8]) -> Result<(StreamSeq, Option<EngineSeq>, Event), RejectReason> {
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

    let (engine_seq, event) = match tag {
        TAG_ACCEPTED => (
            Some(EngineSeq(read_u64(frame, 9))),
            Event::Accepted {
                account_id: AccountId(read_u64(frame, 17)),
                order_id: OrderId(read_u64(frame, 25)),
                resting_qty: Qty(read_u64(frame, 33)),
            },
        ),
        TAG_REJECTED => (
            Some(EngineSeq(read_u64(frame, 9))),
            Event::Rejected {
                account_id: AccountId(read_u64(frame, 17)),
                order_id: OrderId(read_u64(frame, 25)),
                reason: reject_reason_from_u8(read_u8(frame, 33))?,
            },
        ),
        TAG_FILLED => (
            Some(EngineSeq(read_u64(frame, 9))),
            Event::Filled {
                account_id: AccountId(read_u64(frame, 17)),
                order_id: OrderId(read_u64(frame, 25)),
                side: side_from_u8(read_u8(frame, 33))?,
                price: Price(read_u64(frame, 34)),
                qty: Qty(read_u64(frame, 42)),
                resting_qty: Qty(read_u64(frame, 50)),
                state: fill_state_from_u8(read_u8(frame, 58))?,
            },
        ),
        TAG_CANCELLED => (
            Some(EngineSeq(read_u64(frame, 9))),
            Event::Cancelled {
                account_id: AccountId(read_u64(frame, 17)),
                order_id: OrderId(read_u64(frame, 25)),
            },
        ),
        TAG_REPLACED => (
            Some(EngineSeq(read_u64(frame, 9))),
            Event::Replaced {
                account_id: AccountId(read_u64(frame, 17)),
                order_id: OrderId(read_u64(frame, 25)),
                new_qty: Qty(read_u64(frame, 33)),
                priority_retained: read_bool(frame, 41)?,
            },
        ),
        TAG_TRADE => (
            None,
            Event::Trade {
                price: Price(read_u64(frame, 9)),
                qty: Qty(read_u64(frame, 17)),
                taker_side: side_from_u8(read_u8(frame, 25))?,
            },
        ),
        TAG_BOOK_UPDATE => (
            None,
            Event::BookUpdate {
                best_bid: read_bool(frame, 9)?
                    .then(|| (Price(read_u64(frame, 10)), Qty(read_u64(frame, 18)))),
                best_ask: read_bool(frame, 26)?
                    .then(|| (Price(read_u64(frame, 27)), Qty(read_u64(frame, 35)))),
            },
        ),
        TAG_SNAPSHOT_LEVEL => (
            None,
            Event::SnapshotLevel {
                side: side_from_u8(read_u8(frame, 9))?,
                price: Price(read_u64(frame, 10)),
                qty: Qty(read_u64(frame, 18)),
                order_count: read_u64(frame, 26),
            },
        ),
        TAG_SNAPSHOT_ACCOUNT => (
            None,
            Event::SnapshotAccount {
                account_id: AccountId(read_u64(frame, 9)),
                open_order_count: read_u64(frame, 17),
                notional: read_u128(frame, 25),
            },
        ),
        TAG_SNAPSHOT_SUMMARY => (
            None,
            Event::SnapshotSummary {
                best_bid: read_bool(frame, 9)?
                    .then(|| (Price(read_u64(frame, 10)), Qty(read_u64(frame, 18)))),
                best_ask: read_bool(frame, 26)?
                    .then(|| (Price(read_u64(frame, 27)), Qty(read_u64(frame, 35)))),
                last_trade: read_bool(frame, 43)?.then(|| Price(read_u64(frame, 44))),
                level_count: read_u64(frame, 52),
                account_count: read_u64(frame, 60),
            },
        ),
        _ => unreachable!("message_len already rejected any tag not handled above"),
    };
    Ok((seq, engine_seq, event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::types::{FillState, Side};

    fn roundtrip(seq: u64, engine_seq: Option<u64>, event: Event) {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let len = encode_event(StreamSeq(seq), engine_seq.map(EngineSeq), &event, &mut buf);
        let (decoded_seq, decoded_engine_seq, decoded_event) =
            decode_event(&buf[..len]).expect("encoded event must decode");
        assert_eq!(decoded_seq, StreamSeq(seq));
        assert_eq!(decoded_engine_seq, engine_seq.map(EngineSeq));
        assert_eq!(decoded_event, event);
    }

    #[test]
    fn accepted_round_trips() {
        roundtrip(
            1,
            Some(100),
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
            Some(101),
            Event::Rejected {
                account_id: AccountId(1),
                order_id: OrderId(2),
                reason: RejectReason::PriceBandViolation,
            },
        );
        roundtrip(
            2,
            Some(102),
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
            Some(103),
            Event::Filled {
                account_id: AccountId(1),
                order_id: OrderId(2),
                side: Side::Sell,
                price: Price(100),
                qty: Qty(4),
                resting_qty: Qty(1),
                state: FillState::PartiallyFilled,
            },
        );
        roundtrip(
            3,
            Some(104),
            Event::Filled {
                account_id: AccountId(1),
                order_id: OrderId(2),
                side: Side::Sell,
                price: Price(100),
                qty: Qty(4),
                resting_qty: Qty(0),
                state: FillState::Filled,
            },
        );
    }

    #[test]
    fn cancelled_round_trips() {
        roundtrip(
            4,
            Some(105),
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
            Some(106),
            Event::Replaced {
                account_id: AccountId(1),
                order_id: OrderId(2),
                new_qty: Qty(3),
                priority_retained: true,
            },
        );
        roundtrip(
            6,
            Some(107),
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
            None,
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
            None,
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
            None,
            Event::BookUpdate {
                best_bid: None,
                best_ask: None,
            },
        );
    }

    #[test]
    fn snapshot_level_round_trips() {
        roundtrip(
            10,
            None,
            Event::SnapshotLevel {
                side: Side::Buy,
                price: Price(100),
                qty: Qty(5),
                order_count: 3,
            },
        );
    }

    #[test]
    fn snapshot_account_round_trips() {
        roundtrip(
            11,
            None,
            Event::SnapshotAccount {
                account_id: AccountId(1),
                open_order_count: 2,
                notional: 340_282_366_920_938_463_463_374_607_431_768_211_455u128,
            },
        );
    }

    #[test]
    fn snapshot_summary_round_trips() {
        roundtrip(
            12,
            None,
            Event::SnapshotSummary {
                best_bid: Some((Price(99), Qty(10))),
                best_ask: Some((Price(101), Qty(7))),
                last_trade: Some(Price(100)),
                level_count: 4,
                account_count: 2,
            },
        );
        roundtrip(
            13,
            None,
            Event::SnapshotSummary {
                best_bid: None,
                best_ask: None,
                last_trade: None,
                level_count: 0,
                account_count: 0,
            },
        );
    }

    #[test]
    fn truncated_frame_is_rejected() {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let len = encode_event(
            StreamSeq(1),
            Some(EngineSeq(1)),
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
            Some(EngineSeq(1)),
            &Event::Rejected {
                account_id: AccountId(1),
                order_id: OrderId(2),
                reason: RejectReason::ZeroQuantity,
            },
            &mut buf,
        );
        buf[33] = 200; // reason byte, out of range
        assert_eq!(
            decode_event(&buf[..REJECTED_LEN]),
            Err(RejectReason::MalformedMessage)
        );
    }

    #[test]
    fn out_of_range_fill_state_is_rejected() {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        encode_event(
            StreamSeq(1),
            Some(EngineSeq(1)),
            &Event::Filled {
                account_id: AccountId(1),
                order_id: OrderId(2),
                side: Side::Buy,
                price: Price(100),
                qty: Qty(1),
                resting_qty: Qty(0),
                state: FillState::Filled,
            },
            &mut buf,
        );
        buf[58] = 200; // state byte, out of range
        assert_eq!(
            decode_event(&buf[..FILLED_LEN]),
            Err(RejectReason::MalformedMessage)
        );
    }
}
