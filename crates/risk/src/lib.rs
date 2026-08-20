//! Kill switch, per-account limits, price band.
//!
//! Depends on `core` only. Checked on the matching thread before
//! `Engine::apply`, on every command (SPEC §5). Built in stage 4.
