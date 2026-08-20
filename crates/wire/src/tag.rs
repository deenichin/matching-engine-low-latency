//! Message type tags and the fixed length each implies (SPEC §3: "The tag
//! determines total message length, so no separate length prefix is
//! needed"). Inbound and outbound tags share one namespace — each value is
//! globally distinct — so a single lookup serves the framer regardless of
//! which direction it's framing.

pub const TAG_NEW_ORDER: u8 = 1;
pub const TAG_CANCEL_ORDER: u8 = 2;
pub const TAG_CANCEL_REPLACE: u8 = 3;
pub const TAG_MASS_CANCEL: u8 = 4;
pub const TAG_KILL_SWITCH: u8 = 5;
pub const TAG_SNAPSHOT: u8 = 6;

pub const TAG_ACCEPTED: u8 = 10;
pub const TAG_REJECTED: u8 = 11;
pub const TAG_FILLED: u8 = 12;
pub const TAG_CANCELLED: u8 = 13;
pub const TAG_REPLACED: u8 = 14;
pub const TAG_TRADE: u8 = 15;
pub const TAG_BOOK_UPDATE: u8 = 16;

// Inbound. Layout: byte 0 tag, then fixed-width fields in the order listed.
/// `NewOrder`: order_id(8) account_id(8) side(1) price(8) qty(8) order_kind(1) tif(1) client_ts(8)
pub const NEW_ORDER_LEN: usize = 44;
/// `CancelOrder`: order_id(8) account_id(8)
pub const CANCEL_ORDER_LEN: usize = 17;
/// `CancelReplace`: order_id(8) account_id(8) new_price(8) new_qty(8)
pub const CANCEL_REPLACE_LEN: usize = 33;
/// `MassCancel`: account_id(8)
pub const MASS_CANCEL_LEN: usize = 9;
/// `KillSwitch`: engaged(1)
pub const KILL_SWITCH_LEN: usize = 2;
/// `Snapshot`: (no body)
pub const SNAPSHOT_LEN: usize = 1;

// Outbound. Layout: byte 0 tag, byte 1..9 stream_seq(8), then the event's
// own fields — stream_seq is uniform across every outbound message because
// every outbound message belongs to exactly one sequenced stream (SPEC §2,
// §8), even though `core::Event` itself carries no seq field (core doesn't
// know about streams; the caller supplies the seq for the stream it's
// encoding onto).
/// `Accepted`: stream_seq(8) account_id(8) order_id(8) resting_qty(8)
pub const ACCEPTED_LEN: usize = 33;
/// `Rejected`: stream_seq(8) account_id(8) order_id(8) reason(1)
pub const REJECTED_LEN: usize = 26;
/// `Filled`: stream_seq(8) account_id(8) order_id(8) side(1) price(8) qty(8) resting_qty(8)
pub const FILLED_LEN: usize = 50;
/// `Cancelled`: stream_seq(8) account_id(8) order_id(8)
pub const CANCELLED_LEN: usize = 25;
/// `Replaced`: stream_seq(8) account_id(8) order_id(8) new_qty(8) priority_retained(1)
pub const REPLACED_LEN: usize = 34;
/// `Trade`: stream_seq(8) price(8) qty(8) taker_side(1)
pub const TRADE_LEN: usize = 26;
/// `BookUpdate`: stream_seq(8) bid_present(1) bid_price(8) bid_qty(8) ask_present(1) ask_price(8) ask_qty(8)
pub const BOOK_UPDATE_LEN: usize = 43;

/// The largest fixed length any message type can have — sizing guidance
/// for a caller-owned scratch buffer, not a runtime bound.
pub const MAX_MESSAGE_LEN: usize = FILLED_LEN;

/// The total byte length implied by a tag, or `None` if the tag is
/// unrecognized. The framer uses this to know how many bytes a message
/// needs without a separate length prefix.
pub fn message_len(tag: u8) -> Option<usize> {
    match tag {
        TAG_NEW_ORDER => Some(NEW_ORDER_LEN),
        TAG_CANCEL_ORDER => Some(CANCEL_ORDER_LEN),
        TAG_CANCEL_REPLACE => Some(CANCEL_REPLACE_LEN),
        TAG_MASS_CANCEL => Some(MASS_CANCEL_LEN),
        TAG_KILL_SWITCH => Some(KILL_SWITCH_LEN),
        TAG_SNAPSHOT => Some(SNAPSHOT_LEN),
        TAG_ACCEPTED => Some(ACCEPTED_LEN),
        TAG_REJECTED => Some(REJECTED_LEN),
        TAG_FILLED => Some(FILLED_LEN),
        TAG_CANCELLED => Some(CANCELLED_LEN),
        TAG_REPLACED => Some(REPLACED_LEN),
        TAG_TRADE => Some(TRADE_LEN),
        TAG_BOOK_UPDATE => Some(BOOK_UPDATE_LEN),
        _ => None,
    }
}
