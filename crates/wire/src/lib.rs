//! Binary wire protocol: message schemas, encode/decode, framing.
//!
//! Depends on `core` and decodes directly into its types — no parallel
//! type hierarchy, no conversion layer (SPEC §4). Fixed-offset,
//! SBE-inspired, little-endian, one-byte type tag determining message
//! length (SPEC §3) — no separators, no length prefix.

mod codec;
mod framer;
mod inbound;
mod outbound;
mod tag;

pub use framer::Framer;
pub use inbound::{decode_command, encode_command};
pub use outbound::{decode_event, encode_event};
pub use tag::{MAX_MESSAGE_LEN, message_len};
