//! Produces the sample recording shipped at `recordings/sample.bin`
//! (SPEC §7: "a recorded stream and the replay command ship in the repo").
//!
//! This is the file's provenance, not a test: run
//! `cargo run -p bin --example generate_sample_recording` to regenerate it.
//! The sequence is deliberately small and readable rather than pulled from
//! a live capture, since the recording format only cares that it is a
//! valid, decoded, post-gateway-validation `Command` sequence (SPEC §7) —
//! which this constructs directly, the same way the gateway would have
//! decoded it off the wire.

use core::event::Command;
use core::types::{AccountId, OrderId, OrderKind, Price, Qty, Side, Tif};

fn new_order(account: u64, order: u64, side: Side, price: u64, qty: u64, tif: Tif) -> Command {
    Command::NewOrder {
        account_id: AccountId(account),
        order_id: OrderId(order),
        side,
        price: Price(price),
        qty: Qty(qty),
        kind: OrderKind::Limit,
        tif,
        client_ts: 0,
    }
}

fn main() {
    let commands: Vec<Command> = vec![
        // Two resting makers on different accounts, different prices.
        new_order(1, 1, Side::Sell, 100, 5, Tif::Gtc),
        new_order(2, 1, Side::Sell, 101, 3, Tif::Gtc),
        // A full cross against the first maker: a real Trade and
        // BookUpdate, both makers' and the taker's own execution reports.
        new_order(3, 1, Side::Buy, 100, 5, Tif::Gtc),
        // Cancels account 2's still-resting order via mass-cancel.
        Command::MassCancel {
            account_id: AccountId(2),
        },
        // Kill switch engaged: the next NewOrder is risk-rejected, not
        // silently accepted -- this is the case stage 6's replay
        // byte-identical comparison specifically has to reproduce.
        Command::KillSwitch { engaged: true },
        new_order(4, 1, Side::Buy, 100, 1, Tif::Gtc),
        // Disengaged again, showing recovery.
        Command::KillSwitch { engaged: false },
    ];

    let path = std::path::Path::new("recordings/sample.bin");
    std::fs::create_dir_all(
        path.parent()
            .expect("recordings/sample.bin always has a parent"),
    )
    .expect("failed to create recordings/ directory");
    let mut file = std::fs::File::create(path).expect("failed to create recordings/sample.bin");

    use std::io::Write;
    for cmd in &commands {
        let mut buf = [0u8; wire::MAX_MESSAGE_LEN];
        let len = wire::encode_command(cmd, &mut buf);
        file.write_all(&buf[..len])
            .expect("failed to write to recordings/sample.bin");
    }

    println!("wrote {} commands to {}", commands.len(), path.display());
}
