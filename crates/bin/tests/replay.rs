//! Stage 6 replay tests (SPEC §7): `bin::replay_file` and the actual
//! `replay` binary.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use core::error::RejectReason;
use core::event::{Command, Event};
use core::types::{AccountId, OrderId, OrderKind, Price, Qty, Side, Tif};

fn temp_recording_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!(
        "/tmp/bin-replay-test-{}-{n}.bin",
        std::process::id()
    ))
}

fn write_recording(commands: &[Command]) -> PathBuf {
    let path = temp_recording_path();
    let mut file = std::fs::File::create(&path).expect("failed to create temp recording file");
    for cmd in commands {
        let mut buf = [0u8; wire::MAX_MESSAGE_LEN];
        let len = wire::encode_command(cmd, &mut buf);
        file.write_all(&buf[..len])
            .expect("failed to write temp recording");
    }
    path
}

fn new_order(account: u64, order: u64, side: Side, price: u64, qty: u64) -> Command {
    Command::NewOrder {
        account_id: AccountId(account),
        order_id: OrderId(order),
        side,
        price: Price(price),
        qty: Qty(qty),
        kind: OrderKind::Limit,
        tif: Tif::Gtc,
        client_ts: 0,
    }
}

#[test]
fn replay_is_deterministic_across_two_runs() {
    let commands = vec![
        new_order(1, 1, Side::Sell, 100, 5),
        new_order(2, 1, Side::Sell, 101, 3),
        new_order(3, 1, Side::Buy, 100, 5),
        Command::MassCancel {
            account_id: AccountId(2),
        },
    ];
    let recording = write_recording(&commands);

    let a = bin::replay_file(
        &recording,
        std::path::Path::new("/nonexistent/risk.toml"),
        false,
    )
    .expect("first replay must succeed");
    let b = bin::replay_file(
        &recording,
        std::path::Path::new("/nonexistent/risk.toml"),
        false,
    )
    .expect("second replay must succeed");

    assert_eq!(a.execution_reports, b.execution_reports);
    assert_eq!(a.market_data, b.market_data);
    assert_eq!(a.last_trade, b.last_trade);
    assert_eq!(a.last_trade, Some(Price(100)));
    assert!(!a.execution_reports.is_empty());
    assert!(!a.market_data.is_empty());
}

#[test]
fn a_risk_rejection_reproduces_identically_on_replay() {
    let commands = vec![
        Command::KillSwitch { engaged: true },
        new_order(1, 1, Side::Buy, 100, 1),
    ];
    let recording = write_recording(&commands);

    for _ in 0..2 {
        let output = bin::replay_file(
            &recording,
            std::path::Path::new("/nonexistent/risk.toml"),
            false,
        )
        .expect("replay must succeed");
        assert_eq!(
            output.execution_reports,
            vec![(
                core::types::StreamSeq(1),
                Event::Rejected {
                    account_id: AccountId(1),
                    order_id: OrderId(1),
                    reason: RejectReason::KillSwitchActive,
                }
            )]
        );
    }
}

#[test]
fn a_malformed_recording_is_reported_as_an_error_not_a_panic() {
    let path = temp_recording_path();
    std::fs::write(&path, [0xFFu8, 0xFF, 0xFF]).expect("failed to write malformed recording");

    let result = bin::replay_file(&path, std::path::Path::new("/nonexistent/risk.toml"), false);
    assert!(result.is_err());
}

#[test]
fn the_replay_binary_reproduces_the_shipped_sample_byte_identically() {
    let sample = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/bin has a parent")
        .parent()
        .expect("crates/ has a parent")
        .join("recordings/sample.bin");
    assert!(
        sample.exists(),
        "expected the shipped sample recording at {}",
        sample.display()
    );

    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_replay"))
            .arg(&sample)
            .output()
            .expect("failed to run the replay binary")
    };

    let first = run();
    let second = run();

    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(!first.stdout.is_empty());
    assert_eq!(
        first.stdout, second.stdout,
        "two replay runs of the same recording must produce byte-identical output"
    );
}
