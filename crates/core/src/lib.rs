//! Domain types and matching logic for the single-symbol order book.
//!
//! This crate has **zero dependencies** — verified mechanically by
//! `cargo tree -p core` in `check.sh` — and knows nothing about sockets,
//! threads, or bytes (SPEC.md §4). `wire` decodes directly into the types
//! defined here; there is no parallel wire type hierarchy.
//!
//! Matching logic (`Engine`, `Book`, `Arena`, `Level`, `AccountEntry`) is
//! built in stage 1. This crate currently only defines the shapes that the
//! rest of the system is expressed in: domain types, `Command`/`Event`, and
//! the reject-reason taxonomy.

pub mod error;
pub mod event;
pub mod types;

pub use error::RejectReason;
pub use event::{Command, Event};
pub use types::*;
