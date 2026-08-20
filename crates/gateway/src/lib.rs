//! `Transport` trait, UDS implementation, connection management,
//! validation, and the gateway/matching thread topology (SPEC §3, §4).
//! Depends on `wire`, `risk`, `core`.

mod conn;
mod matching;
mod order_entry;
mod transport;

pub use conn::ConnId;
pub use matching::run_matching_thread;
pub use order_entry::run_order_entry;
pub use transport::{Transport, UdsTransport};
