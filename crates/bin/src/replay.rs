//! Deterministic replay (SPEC §7): reads a recorded, post-gateway-
//! validation `Command` sequence and feeds it through the exact same
//! risk-then-matching path live traffic uses (`risk::process_command`) —
//! not a parallel reimplementation. Bypasses only the socket and framing
//! layer, not risk, per SPEC §7's explicit rationale: skipping risk would
//! prove a strictly weaker property, unable to confirm that the kill
//! switch, price band, or notional cap reject identically on replay.

use std::path::Path;

use core::event::Event;
use core::types::{EngineSeq, Price, StreamSeq};
use core::{Command, Engine};
use risk::{RiskConfig, RiskState};

/// The full outbound stream replay produced, split the same way the live
/// system's two streams are shaped — execution reports and market data —
/// plus `last_trade`, verified explicitly per SPEC §7: it is `Book`-
/// internal state with no direct event-stream representation, so a
/// byte-identical comparison of the event stream alone would not by
/// itself prove it replayed correctly.
///
/// Replay has no connections (SPEC §7 bypasses sockets entirely), so
/// unlike live traffic's per-connection execution-report counters, this
/// assigns exactly two independent `StreamSeq` counters — one per stream
/// shape, not one per connection, since there is no connection identity to
/// replay.
#[derive(Debug, PartialEq, Eq)]
pub struct ReplayOutput {
    pub execution_reports: Vec<(StreamSeq, EngineSeq, Event)>,
    pub market_data: Vec<(StreamSeq, Event)>,
    pub last_trade: Option<Price>,
}

/// Reads `recording_path`, decodes it as a sequence of `wire`-framed
/// `Command`s (the same self-framing format live traffic uses on the
/// wire — reused here, not reinvented), and replays it against a fresh
/// `Engine` and fresh `RiskState` loaded from `risk_config_path`.
///
/// `exclude_timestamps` is accepted and documented, not yet implemented:
/// no field on `Command` or `Event` carries a system-stamped timestamp
/// today (SPEC §3's `recv_ts` is a tracked gap, not built in this stage —
/// see the note next to it in SPEC.md). Once `recv_ts` exists, this flag
/// is where its exclusion from the comparison belongs; today it is a
/// no-op, kept on the signature so that fact is visible at every call
/// site rather than silently true.
///
/// Never panics on a bad path or malformed recording — both are supplied
/// by the caller (a CLI argument, a possibly-corrupted file), not an
/// internal invariant.
pub fn replay_file(
    recording_path: &Path,
    risk_config_path: &Path,
    _exclude_timestamps: bool,
) -> Result<ReplayOutput, String> {
    let bytes = std::fs::read(recording_path)
        .map_err(|e| format!("failed to read {}: {e}", recording_path.display()))?;

    let mut framer = wire::Framer::new();
    framer.feed(&bytes);
    let mut commands: Vec<Command> = Vec::new();
    let mut decode_error: Option<String> = None;
    let frame_result = framer.drain_frames(|frame| match wire::decode_command(frame) {
        Ok(cmd) => commands.push(cmd),
        Err(reason) => {
            decode_error
                .get_or_insert_with(|| format!("malformed command in recording: {reason:?}"));
        }
    });
    if let Err(reason) = frame_result {
        return Err(format!(
            "recording contains an unrecognized message tag: {reason:?}"
        ));
    }
    if let Some(err) = decode_error {
        return Err(err);
    }

    let risk_config = RiskConfig::load(risk_config_path);
    let mut engine = Engine::new();
    let mut risk_state = RiskState::new(risk_config);

    let mut execution_reports = Vec::new();
    let mut market_data = Vec::new();
    let mut exec_seq = 0u64;
    let mut md_seq = 0u64;
    // Fresh per run, exactly like exec_seq/md_seq above -- and, crucially,
    // incremented by the exact same function (risk::process_command) the
    // live matching thread calls, so a replayed run's EngineSeq values
    // cannot drift from a live run's by construction (SPEC §2).
    let mut engine_seq = EngineSeq(0);

    for cmd in commands {
        risk::process_command(
            &mut engine,
            &mut risk_state,
            cmd,
            &mut engine_seq,
            &mut |seq, event| {
                if matches!(event, Event::Trade { .. } | Event::BookUpdate { .. }) {
                    md_seq += 1;
                    market_data.push((StreamSeq(md_seq), event));
                } else {
                    exec_seq += 1;
                    execution_reports.push((StreamSeq(exec_seq), seq, event));
                }
            },
        );
    }

    Ok(ReplayOutput {
        execution_reports,
        market_data,
        last_trade: engine.book().last_trade(),
    })
}
