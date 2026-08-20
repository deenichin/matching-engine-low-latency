//! The daemon binary. All the actual wiring lives in `lib.rs`, so
//! integration tests can call it against temporary socket paths too.

use std::path::{Path, PathBuf};

/// Order-entry socket path (SPEC §3).
const ORDER_ENTRY_SOCKET: &str = "/run/engine/order-entry.sock";

/// Market-data socket path (SPEC §3).
const MARKET_DATA_SOCKET: &str = "/run/engine/market-data.sock";

/// Risk config path (SPEC §5), relative to the daemon's working
/// directory — `/home/app/risk.toml` in the Docker image ("no host setup
/// required"), the repo root when run locally. Missing entirely still
/// works: `RiskConfig::load` falls back to the SPEC §5 defaults.
const RISK_CONFIG_PATH: &str = "risk.toml";

/// `--record <path>` (SPEC §7, stage 6): opt-in recording of the
/// post-gateway-validation `Command` sequence, for later replay. No
/// CLI-parser dependency — hand-rolled, consistent with stage 4's
/// `risk.toml` parser.
fn parse_record_path(args: &[String]) -> Option<PathBuf> {
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--record" {
            let Some(path) = args.get(i + 1) else {
                eprintln!("--record requires a path argument");
                std::process::exit(1);
            };
            return Some(PathBuf::from(path));
        }
        i += 1;
    }
    None
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let record_path = parse_record_path(&args);

    bin::run(
        Path::new(ORDER_ENTRY_SOCKET),
        Path::new(MARKET_DATA_SOCKET),
        Path::new(RISK_CONFIG_PATH),
        record_path.as_deref(),
    );
}
