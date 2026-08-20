//! Top-of-book and trade publishing.
//!
//! Depends on `wire`, `core`. Owns the market-data socket and its own
//! thread (SPEC §4, §8).

mod queue;
mod subscriber;

pub use subscriber::{SubscriberId, bind, run_market_data};
