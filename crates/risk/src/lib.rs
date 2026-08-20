//! Kill switch, per-account limits, price band.
//!
//! Depends on `core` only. Checked on the matching thread before
//! `Engine::apply`, on every command (SPEC §5).
//!
//! **Stage 3 stub.** The matching thread's loop needs a kill-switch/risk
//! gate to call before `apply` regardless of whether stage 4 has landed
//! yet, so that call site doesn't change shape later. [`pre_apply_check`]
//! always passes for now — no kill-switch state, no per-account limits, no
//! price band. Real logic replaces the body in stage 4; the signature is
//! deliberately already what stage 4 needs.

use core::event::{Command, Event};

/// Pre-`apply` gate: kill switch, per-account limits, price band.
/// `Some(reject)` means the command must never reach `Engine::apply`;
/// `None` means proceed.
///
/// Always `None` until stage 4. Not real risk logic — the hook the
/// matching thread calls.
pub fn pre_apply_check(_cmd: &Command) -> Option<Event> {
    None
}
