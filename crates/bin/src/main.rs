//! The daemon binary. All the actual wiring lives in `lib.rs`, so
//! integration tests can call it against temporary socket paths too.

use std::path::Path;

/// Order-entry socket path (SPEC §3).
const ORDER_ENTRY_SOCKET: &str = "/run/engine/order-entry.sock";

/// Market-data socket path (SPEC §3).
const MARKET_DATA_SOCKET: &str = "/run/engine/market-data.sock";

/// Risk config path (SPEC §5), relative to the daemon's working
/// directory — `/home/app/risk.toml` in the Docker image ("no host setup
/// required"), the repo root when run locally. Missing entirely still
/// works: `RiskConfig::load` falls back to the SPEC §5 defaults.
const RISK_CONFIG_PATH: &str = "risk.toml";

fn main() {
    bin::run(
        Path::new(ORDER_ENTRY_SOCKET),
        Path::new(MARKET_DATA_SOCKET),
        Path::new(RISK_CONFIG_PATH),
    );
}
