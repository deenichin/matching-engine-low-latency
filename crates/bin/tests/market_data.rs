//! Stage 5 requirements exercised end-to-end: the real daemon wiring
//! (`bin::run`), a real order-entry socket, and a real market-data socket
//! with real subscribers.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use core::event::{Command, Event};
use core::types::{AccountId, OrderId, OrderKind, Price, Qty, Side, StreamSeq, Tif};

fn temp_socket_path(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!(
        "/tmp/bin-test-{label}-{}-{n}.sock",
        std::process::id()
    ))
}

/// Spawns the full daemon (order entry + matching + market data) on a
/// background thread, bound at fresh temporary paths, with risk config
/// pointed at a path that doesn't exist -- `RiskConfig::load` falls back
/// to SPEC §5's baked-in defaults, so tests don't depend on the repo's
/// `risk.toml` or the process's working directory.
fn spawn_daemon() -> (PathBuf, PathBuf) {
    let order_entry_path = temp_socket_path("oe");
    let market_data_path = temp_socket_path("md");
    let risk_config_path = PathBuf::from("/nonexistent/risk.toml");

    let oe = order_entry_path.clone();
    let md = market_data_path.clone();
    std::thread::spawn(move || bin::run(&oe, &md, &risk_config_path, None));

    (order_entry_path, market_data_path)
}

/// Connects, retrying for a few seconds -- `bin::run`'s binds happen on
/// the spawned thread, so the listener may not exist yet the instant
/// `spawn_daemon` returns.
fn connect_with_retry(path: &PathBuf) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return stream,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => panic!("failed to connect to {}: {e}", path.display()),
        }
    }
}

fn send_new_order(
    client: &mut UnixStream,
    account_id: u64,
    order_id: u64,
    side: Side,
    price: u64,
    qty: u64,
) {
    let mut buf = [0u8; wire::MAX_MESSAGE_LEN];
    let len = wire::encode_command(
        &Command::NewOrder {
            account_id: AccountId(account_id),
            order_id: OrderId(order_id),
            side,
            price: Price(price),
            qty: Qty(qty),
            kind: OrderKind::Limit,
            tif: Tif::Gtc,
            client_ts: 0,
        },
        &mut buf,
    );
    client.write_all(&buf[..len]).expect("client write failed");
}

fn send_cancel(client: &mut UnixStream, account_id: u64, order_id: u64) {
    let mut buf = [0u8; wire::MAX_MESSAGE_LEN];
    let len = wire::encode_command(
        &Command::CancelOrder {
            account_id: AccountId(account_id),
            order_id: OrderId(order_id),
        },
        &mut buf,
    );
    client.write_all(&buf[..len]).expect("client write failed");
}

/// Reads exactly one frame and decodes it -- the tag byte determines the
/// rest of the length (SPEC §3).
fn read_one(stream: &mut UnixStream) -> (StreamSeq, Event) {
    let mut tag_buf = [0u8; 1];
    stream
        .read_exact(&mut tag_buf)
        .expect("failed to read tag byte");
    let len = wire::message_len(tag_buf[0]).expect("server sent an unrecognized tag");
    let mut frame = vec![0u8; len];
    frame[0] = tag_buf[0];
    stream
        .read_exact(&mut frame[1..])
        .expect("failed to read the rest of the frame");
    wire::decode_event(&frame).expect("failed to decode the server's frame")
}

#[test]
fn slow_subscriber_does_not_cause_the_fast_subscriber_to_lose_messages() {
    let (order_entry_path, market_data_path) = spawn_daemon();

    let mut fast = connect_with_retry(&market_data_path);
    fast.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    // Connected but never read from -- its own queue will overflow and
    // drop internally, which is exactly what must NOT affect `fast`.
    let _slow = connect_with_retry(&market_data_path);

    let mut client = connect_with_retry(&order_entry_path);
    // A large resting maker so repeated small crossings only ever reduce
    // its qty (never empty the level) -- every fill changes the qty at
    // the touch, so every fill produces exactly one Trade and one
    // BookUpdate: a precisely predictable message count.
    // Notional = price * qty must stay comfortably under the default
    // risk config's max_notional (100_000_000 at price 100 => qty must
    // stay well under 1_000_000).
    send_new_order(&mut client, 1, 1, Side::Sell, 100, 500_000);
    read_one(&mut client); // Accepted

    const N: u64 = 20;
    for i in 0..N {
        send_new_order(&mut client, 2, 1 + i, Side::Buy, 100, 1);
        read_one(&mut client); // Filled
        read_one(&mut client); // Accepted
    }

    // Exactly 2N messages (Trade + BookUpdate per fill), with a
    // perfectly contiguous StreamSeq -- the fast subscriber lost nothing,
    // regardless of what the slow one is doing.
    let mut seqs = Vec::new();
    for _ in 0..(2 * N) {
        let (seq, _event) = read_one(&mut fast);
        seqs.push(seq.0);
    }
    let expected: Vec<u64> = (1..=2 * N).collect();
    assert_eq!(
        seqs, expected,
        "fast subscriber must see every message with no gaps"
    );
}

#[test]
fn execution_report_only_burst_does_not_advance_market_data_stream_seq() {
    let (order_entry_path, market_data_path) = spawn_daemon();

    let mut subscriber = connect_with_retry(&market_data_path);
    subscriber
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();

    let mut client = connect_with_retry(&order_entry_path);
    // A burst of commands that only ever produce execution reports: a
    // cancel for an order that was never submitted touches nothing in
    // the book at all.
    for i in 0..10 {
        send_cancel(&mut client, 1, i);
        let (_seq, event) = read_one(&mut client);
        assert!(matches!(event, Event::Rejected { .. }));
    }

    // Nothing must have arrived on the market-data subscriber.
    let mut probe = [0u8; 1];
    let result = subscriber.read(&mut probe);
    let is_would_block_or_timeout = matches!(
        &result,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut
    );
    assert!(
        is_would_block_or_timeout,
        "an execution-report-only burst must not deliver anything to market data, got {result:?}"
    );

    // A real book change now -- its market-data message must arrive as
    // seq 1, proving the counter never moved during the earlier burst.
    send_new_order(&mut client, 1, 1, Side::Buy, 100, 5);
    read_one(&mut client); // Accepted, on the order-entry connection

    subscriber
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (seq, event) = read_one(&mut subscriber);
    assert_eq!(seq, StreamSeq(1));
    assert!(matches!(event, Event::BookUpdate { .. }));
}

#[test]
fn slow_market_data_subscriber_does_not_stall_matching_or_order_entry() {
    let (order_entry_path, market_data_path) = spawn_daemon();

    // Connected but never read from. Its OS-level socket receive buffer
    // will fill, then this server's write to it will actually block --
    // in that subscriber's own writer thread only.
    let _slow_subscriber = connect_with_retry(&market_data_path);

    let mut maker = connect_with_retry(&order_entry_path);
    // Notional must stay under the default risk config's max_notional
    // (100_000_000 at price 100 => qty well under 1_000_000).
    send_new_order(&mut maker, 1, 1, Side::Sell, 100, 500_000);
    read_one(&mut maker); // Accepted

    // The maker's single resting order gets partially filled on every
    // taker crossing below, so its own connection receives one Filled
    // per iteration too. Drain it on a background thread -- an
    // execution-report connection that stops reading is a real stall
    // this test isn't about, and would otherwise mask the thing it's
    // actually trying to prove.
    std::thread::spawn(move || {
        let mut tag_buf = [0u8; 1];
        while maker.read_exact(&mut tag_buf).is_ok() {
            let Some(len) = wire::message_len(tag_buf[0]) else {
                break;
            };
            let mut frame = vec![0u8; len];
            frame[0] = tag_buf[0];
            if maker.read_exact(&mut frame[1..]).is_err() {
                break;
            }
        }
    });

    // Enough volume to comfortably exceed a UDS socket's default kernel
    // send buffer, so the slow subscriber's writer thread is genuinely
    // blocked on write(), not just sitting on an unfilled in-process
    // queue -- proving the stall is contained, not merely untested.
    const N: u64 = 4000;
    let mut taker = connect_with_retry(&order_entry_path);
    taker
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    let start = Instant::now();
    for i in 0..N {
        send_new_order(&mut taker, 2, 1 + i, Side::Buy, 100, 1);
        // Both proof points at once: a Filled response means the
        // matching thread actually matched this order (matching
        // progressed), and receiving it promptly over order entry, on a
        // fresh timeout each iteration, means the gateway's read/write
        // path was never stalled by the market-data side (order entry
        // progressed) -- the entire reason market data has its own
        // thread and socket (SPEC §4).
        let (_seq, event) = read_one(&mut taker);
        assert!(
            matches!(event, Event::Filled { .. }),
            "matching thread must keep matching orders"
        );
        let (_seq, event) = read_one(&mut taker);
        assert!(matches!(event, Event::Accepted { .. }));
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(30),
        "order entry took {elapsed:?} for {N} orders with a stalled market-data subscriber \
         connected -- the separation between order entry and market data failed"
    );
}

#[test]
fn both_streams_have_independently_monotonic_sequence_numbers() {
    let (order_entry_path, market_data_path) = spawn_daemon();

    let mut subscriber = connect_with_retry(&market_data_path);
    let mut client = connect_with_retry(&order_entry_path);

    const N: u64 = 10;
    let mut execution_seqs = Vec::new();
    for i in 0..N {
        send_new_order(&mut client, 1, i, Side::Buy, 100 + i, 1);
        let (seq, _event) = read_one(&mut client); // Accepted
        execution_seqs.push(seq.0);
    }

    let mut market_data_seqs = Vec::new();
    for _ in 0..N {
        let (seq, _event) = read_one(&mut subscriber); // BookUpdate, one per new best bid
        market_data_seqs.push(seq.0);
    }

    let is_strictly_increasing = |seqs: &[u64]| seqs.windows(2).all(|w| w[1] > w[0]);
    assert!(
        is_strictly_increasing(&execution_seqs),
        "{execution_seqs:?}"
    );
    assert!(
        is_strictly_increasing(&market_data_seqs),
        "{market_data_seqs:?}"
    );
    // Both start from 1 independently -- neither stream's counter is
    // derived from or offset by the other's.
    assert_eq!(execution_seqs[0], 1);
    assert_eq!(market_data_seqs[0], 1);
}
