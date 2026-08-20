//! CLI wrapper around `bin::replay_file` (SPEC §7): reads a recorded
//! `Command` sequence, replays it through risk-then-matching, and writes
//! the resulting outbound stream's wire-encoded bytes to stdout — so two
//! invocations can be diffed externally (`diff`, `sha256sum`) as literal
//! proof of "byte-identical," not just a claim.
//!
//! Usage: `replay <recording-path> [--risk-config <path>] [--exclude-timestamps]`

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use core::types::StreamSeq;

const DEFAULT_RISK_CONFIG_PATH: &str = "risk.toml";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut recording_path: Option<PathBuf> = None;
    let mut risk_config_path = PathBuf::from(DEFAULT_RISK_CONFIG_PATH);
    let mut exclude_timestamps = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--risk-config" => {
                i += 1;
                let Some(path) = args.get(i) else {
                    eprintln!("--risk-config requires a path argument");
                    std::process::exit(1);
                };
                risk_config_path = PathBuf::from(path);
            }
            "--exclude-timestamps" => exclude_timestamps = true,
            other if recording_path.is_none() && !other.starts_with("--") => {
                recording_path = Some(PathBuf::from(other));
            }
            other => {
                eprintln!("unrecognized argument: {other}");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    let Some(recording_path) = recording_path else {
        eprintln!("usage: replay <recording-path> [--risk-config <path>] [--exclude-timestamps]");
        std::process::exit(1);
    };

    let output = match bin::replay_file(&recording_path, &risk_config_path, exclude_timestamps) {
        Ok(output) => output,
        Err(e) => {
            eprintln!("replay failed: {e}");
            std::process::exit(1);
        }
    };

    write_output(&output);
}

/// Writes every event's wire-encoded bytes to stdout, execution reports
/// first then market data — a fixed, deterministic order so two runs of
/// the same recording produce byte-identical stdout regardless of how the
/// events happened to interleave internally.
fn write_output(output: &bin::ReplayOutput) {
    let stdout = std::io::stdout();
    if stdout.is_terminal() {
        eprintln!(
            "note: writing {} execution-report and {} market-data events as wire bytes to stdout; redirect to a file to inspect or diff",
            output.execution_reports.len(),
            output.market_data.len()
        );
    }
    let mut out = stdout.lock();
    for (seq, event) in output.execution_reports.iter().chain(&output.market_data) {
        write_one(&mut out, *seq, event);
    }
}

fn write_one(out: &mut impl Write, seq: StreamSeq, event: &core::Event) {
    let mut buf = [0u8; wire::MAX_MESSAGE_LEN];
    let len = wire::encode_event(seq, event, &mut buf);
    let _ = out.write_all(&buf[..len]);
}
