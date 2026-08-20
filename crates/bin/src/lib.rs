//! The daemon's wiring, as a library function so both `main.rs` and
//! integration tests can call it against real or temporary socket paths
//! (SPEC §4): `gateway`'s order-entry and matching threads, `risk`'s
//! state, and `marketdata`'s subscriber thread, all wired together.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::mpsc;

use gateway::{Transport, UdsTransport, run_matching_thread, run_order_entry};
use risk::{RiskConfig, RiskState};

mod replay;
pub use replay::{ReplayOutput, replay_file};

/// Placeholder channel capacities. Bounded and pre-allocated, per
/// CLAUDE.md — the actual numbers are a stage 7 benchmarking concern.
const COMMAND_CHANNEL_CAPACITY: usize = 1024;
const RETURN_CHANNEL_CAPACITY: usize = 1024;
const MARKET_DATA_CHANNEL_CAPACITY: usize = 1024;

/// Binds both sockets, wires every thread together, and blocks on the
/// order-entry accept loop (the same shape `main` runs with, against
/// whatever paths `main` or a test supplies). Exits the process on a bind
/// failure — a test wanting to assert on that failure instead should call
/// `UdsTransport::bind`/`marketdata::bind` itself rather than go through
/// this function.
///
/// `record_path`, when `Some`, opens that file and passes it to the
/// matching thread as its stage-6 recording sink (SPEC §7) — the
/// post-gateway-validation, pre-risk `Command` sequence. `None` (the
/// default) records nothing, at zero cost to the hot path.
pub fn run(
    order_entry_path: &Path,
    market_data_path: &Path,
    risk_config_path: &Path,
    record_path: Option<&Path>,
) {
    let transport = match UdsTransport::bind(order_entry_path) {
        Ok(transport) => transport,
        Err(e) => {
            eprintln!(
                "failed to bind order-entry socket at {}: {e}",
                order_entry_path.display()
            );
            std::process::exit(1);
        }
    };

    let market_data_listener = match marketdata::bind(market_data_path) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!(
                "failed to bind market-data socket at {}: {e}",
                market_data_path.display()
            );
            std::process::exit(1);
        }
    };

    let risk_config = RiskConfig::load(risk_config_path);
    let risk_state = RiskState::new(risk_config);

    let recorder = record_path.map(|path| match File::create(path) {
        Ok(file) => BufWriter::new(file),
        Err(e) => {
            eprintln!("failed to open recording file at {}: {e}", path.display());
            std::process::exit(1);
        }
    });

    let (command_tx, command_rx) = mpsc::sync_channel(COMMAND_CHANNEL_CAPACITY);
    let (return_tx, return_rx) = mpsc::sync_channel(RETURN_CHANNEL_CAPACITY);
    let (market_data_tx, market_data_rx) = mpsc::sync_channel(MARKET_DATA_CHANNEL_CAPACITY);

    let return_tx_for_matching = return_tx.clone();
    let matching_handle = std::thread::spawn(move || {
        run_matching_thread(
            command_rx,
            return_tx_for_matching,
            market_data_tx,
            risk_state,
            recorder,
        )
    });

    let market_data_handle = std::thread::spawn(move || {
        marketdata::run_market_data(market_data_listener, market_data_rx)
    });

    println!(
        "matching-engine: order entry on {}, market data on {}",
        order_entry_path.display(),
        market_data_path.display()
    );
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
    let _ = market_data_handle.join();
}
