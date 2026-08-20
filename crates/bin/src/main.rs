//! The daemon. Wires `gateway`'s order-entry and matching threads, and
//! `risk`'s state, together (SPEC §4). `marketdata` joins in stage 5.

use std::path::Path;
use std::sync::mpsc;

use gateway::{Transport, UdsTransport, run_matching_thread, run_order_entry};
use risk::{RiskConfig, RiskState};

/// Order-entry socket path (SPEC §3).
const ORDER_ENTRY_SOCKET: &str = "/run/engine/order-entry.sock";

/// Risk config path (SPEC §5), relative to the daemon's working
/// directory — `/home/app/risk.toml` in the Docker image ("no host setup
/// required"), the repo root when run locally. Missing entirely still
/// works: `RiskConfig::load` falls back to the SPEC §5 defaults.
const RISK_CONFIG_PATH: &str = "risk.toml";

/// Placeholder channel capacities. Bounded and pre-allocated, per
/// CLAUDE.md — the actual numbers are a stage 7 benchmarking concern, not
/// a stage 3 one.
const COMMAND_CHANNEL_CAPACITY: usize = 1024;
const RETURN_CHANNEL_CAPACITY: usize = 1024;
const MARKET_DATA_CHANNEL_CAPACITY: usize = 1024;

fn main() {
    let path = Path::new(ORDER_ENTRY_SOCKET);
    let transport = match UdsTransport::bind(path) {
        Ok(transport) => transport,
        Err(e) => {
            eprintln!(
                "failed to bind order-entry socket at {}: {e}",
                path.display()
            );
            std::process::exit(1);
        }
    };

    let risk_config = RiskConfig::load(Path::new(RISK_CONFIG_PATH));
    let risk_state = RiskState::new(risk_config);

    let (command_tx, command_rx) = mpsc::sync_channel(COMMAND_CHANNEL_CAPACITY);
    let (return_tx, return_rx) = mpsc::sync_channel(RETURN_CHANNEL_CAPACITY);
    // Stage 5 stub: nothing drains this yet (SPEC §4 -- the market data
    // thread arrives with its own socket then). Held here rather than
    // dropped, so a Trade/BookUpdate try_send from the matching thread
    // sees a full-but-connected channel, not an immediately-disconnected
    // one -- moot today, since nothing in Book emits either variant yet.
    let (market_data_tx, _market_data_rx) = mpsc::sync_channel(MARKET_DATA_CHANNEL_CAPACITY);

    let return_tx_for_matching = return_tx.clone();
    let matching_handle = std::thread::spawn(move || {
        run_matching_thread(
            command_rx,
            return_tx_for_matching,
            market_data_tx,
            risk_state,
        )
    });

    println!("matching-engine: listening on {}", path.display());
    println!(
        "matching-engine: risk config max_open_orders={} max_notional={} price_band_pct={}",
        risk_config.max_open_orders, risk_config.max_notional, risk_config.price_band_pct
    );

    // The order-entry accept loop is the blocking call that keeps the
    // process alive; it only returns once the transport's listener stops
    // accepting, which nothing currently triggers. Signal-based graceful
    // shutdown (SIGINT/SIGTERM closing the transport deliberately, so
    // UdsTransport's Drop unlinks the socket file before exit) is not
    // wired up: std has no signal handling without either a small
    // dependency (e.g. `ctrlc`) or unsafe libc FFI, and CLAUDE.md reserves
    // unsafe for FFI/hardware boundaries with CPU affinity named as the
    // only current candidate -- so this was left as an explicit decision
    // rather than added silently. The system already tolerates an
    // unclean stop either way: the next bind() removes whatever stale
    // socket file is left behind (SPEC §3).
    run_order_entry(transport, command_tx, return_tx, return_rx);

    let _ = matching_handle.join();
}
