//! Domain types and matching logic for the single-symbol order book.
//!
//! This crate has **zero dependencies** — verified mechanically by
//! `cargo tree -p core` in `check.sh` — and knows nothing about sockets,
//! threads, or bytes (SPEC.md §4). `wire` decodes directly into the types
//! defined here; there is no parallel wire type hierarchy.
//!
//! `Book`, `Arena`, `Level`, and `AccountEntry` are the data structures
//! from SPEC §4: two price maps, a slot-stable node arena, and the
//! per-account index. Matching logic (`Engine`, `rest`/`cancel`/`modify`)
//! is built in stage 1 part two. This crate currently defines shapes plus
//! `Book::assert_invariants()`, written ahead of the logic it will guard.

pub mod account;
pub mod arena;
pub mod book;
pub mod engine;
pub mod error;
pub mod event;
pub mod level;
pub mod types;

pub use account::AccountEntry;
pub use arena::{Arena, Node};
pub use book::Book;
pub use engine::Engine;
pub use error::RejectReason;
pub use event::{Command, Event};
pub use level::Level;
pub use types::*;
