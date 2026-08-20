//! Stage 3 exit criterion (PLAN.md): an order sent over the socket
//! produces a correct execution report, and a second connection is served
//! independently.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::time::Duration;

use core::event::{Command, Event};
use core::types::{AccountId, OrderId, OrderKind, Price, Qty, Side, StreamSeq, Tif};
use gateway::{Transport, UdsTransport, run_matching_thread, run_order_entry};

fn test_socket_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!("/tmp/wire-gw-e2e-{}-{n}.sock", std::process::id()))
}

/// Spawns the gateway (order-entry + matching threads) bound at a fresh
/// path, wired together exactly as `bin` will wire them, and returns the
/// socket path to connect clients to.
fn spawn_gateway() -> PathBuf {
    let path = test_socket_path();
    let transport = UdsTransport::bind(&path).expect("bind should succeed");

    let (command_tx, command_rx) = mpsc::sync_channel(64);
    let (return_tx, return_rx) = mpsc::sync_channel(64);
    let (market_data_tx, _market_data_rx) = mpsc::sync_channel::<Event>(64);

    let return_tx_for_matching = return_tx.clone();
    let risk_state = risk::RiskState::new(risk::RiskConfig::default());
    std::thread::spawn(move || run_order_entry(transport, command_tx, return_tx, return_rx));
    std::thread::spawn(move || {
        run_matching_thread(
            command_rx,
            return_tx_for_matching,
            market_data_tx,
            risk_state,
            None,
        )
    });

    path
}

/// Like `spawn_gateway`, but also returns the market-data receiver instead
/// of discarding it -- for tests that need to observe `BookUpdate`s
/// directly (checking the book never crosses) rather than only inferring
/// state from execution reports.
fn spawn_gateway_with_market_data() -> (PathBuf, mpsc::Receiver<Event>) {
    let path = test_socket_path();
    let transport = UdsTransport::bind(&path).expect("bind should succeed");

    let (command_tx, command_rx) = mpsc::sync_channel(64);
    let (return_tx, return_rx) = mpsc::sync_channel(64);
    let (market_data_tx, market_data_rx) = mpsc::sync_channel::<Event>(64);

    let return_tx_for_matching = return_tx.clone();
    let risk_state = risk::RiskState::new(risk::RiskConfig::default());
    std::thread::spawn(move || run_order_entry(transport, command_tx, return_tx, return_rx));
    std::thread::spawn(move || {
        run_matching_thread(
            command_rx,
            return_tx_for_matching,
            market_data_tx,
            risk_state,
            None,
        )
    });

    (path, market_data_rx)
}

fn connect(path: &PathBuf) -> UnixStream {
    let stream = UnixStream::connect(path).expect("client connect failed");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("failed to set read timeout");
    stream
}

fn send_new_order(client: &mut UnixStream, account_id: u64, order_id: u64, price: u64, qty: u64) {
    let mut buf = [0u8; wire::MAX_MESSAGE_LEN];
    let len = wire::encode_command(
        &Command::NewOrder {
            account_id: AccountId(account_id),
            order_id: OrderId(order_id),
            side: Side::Buy,
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

fn send_new_order_side(
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

/// Reads exactly one frame's worth of bytes and decodes it -- the tag
/// byte determines the rest of the length (SPEC §3), so this reads the
/// tag first, then exactly as many more bytes as it implies.
fn read_one_event(client: &mut UnixStream) -> (StreamSeq, Event) {
    let mut tag_buf = [0u8; 1];
    client
        .read_exact(&mut tag_buf)
        .expect("failed to read tag byte");
    let len = wire::message_len(tag_buf[0]).expect("server sent an unrecognized tag");
    let mut frame = vec![0u8; len];
    frame[0] = tag_buf[0];
    client
        .read_exact(&mut frame[1..])
        .expect("failed to read the rest of the frame");
    wire::decode_event(&frame).expect("failed to decode the server's frame")
}

#[test]
fn order_sent_over_the_socket_produces_a_correct_execution_report() {
    let path = spawn_gateway();
    let mut client = connect(&path);

    send_new_order(&mut client, 1, 1, 100, 5);
    let (seq, event) = read_one_event(&mut client);

    assert_eq!(seq, StreamSeq(1));
    assert_eq!(
        event,
        Event::Accepted {
            account_id: AccountId(1),
            order_id: OrderId(1),
            resting_qty: Qty(5),
        }
    );
}

#[test]
fn a_second_connection_is_served_independently() {
    let path = spawn_gateway();
    let mut client1 = connect(&path);
    let mut client2 = connect(&path);

    // Different accounts, different prices -- nothing about client 2's
    // order should cross client 1's.
    send_new_order(&mut client1, 1, 1, 100, 5);
    send_new_order(&mut client2, 2, 1, 90, 3);

    let (seq1, event1) = read_one_event(&mut client1);
    let (seq2, event2) = read_one_event(&mut client2);

    // Each connection's stream_seq starts at 1 independently -- execution
    // reports are a per-connection stream (SPEC §2, §3), not one counter
    // shared across every client.
    assert_eq!(seq1, StreamSeq(1));
    assert_eq!(seq2, StreamSeq(1));

    assert_eq!(
        event1,
        Event::Accepted {
            account_id: AccountId(1),
            order_id: OrderId(1),
            resting_qty: Qty(5),
        }
    );
    assert_eq!(
        event2,
        Event::Accepted {
            account_id: AccountId(2),
            order_id: OrderId(1),
            resting_qty: Qty(3),
        }
    );
}

#[test]
fn a_fill_produces_multiple_individually_delivered_events() {
    let path = spawn_gateway();
    let mut maker = connect(&path);
    let mut taker = connect(&path);

    // Maker rests a sell; taker's buy crosses it in full.
    let mut buf = [0u8; wire::MAX_MESSAGE_LEN];
    let len = wire::encode_command(
        &Command::NewOrder {
            account_id: AccountId(1),
            order_id: OrderId(1),
            side: Side::Sell,
            price: Price(100),
            qty: Qty(5),
            kind: OrderKind::Limit,
            tif: Tif::Gtc,
            client_ts: 0,
        },
        &mut buf,
    );
    maker.write_all(&buf[..len]).unwrap();
    let (_seq, maker_accept) = read_one_event(&mut maker);
    assert_eq!(
        maker_accept,
        Event::Accepted {
            account_id: AccountId(1),
            order_id: OrderId(1),
            resting_qty: Qty(5),
        }
    );

    send_new_order(&mut taker, 2, 1, 100, 5);

    // Taker sees its own Filled, then its own Accepted -- two separate
    // frames, not one message batching both.
    let (taker_seq_1, taker_filled) = read_one_event(&mut taker);
    let (taker_seq_2, taker_accept) = read_one_event(&mut taker);
    assert_eq!(taker_seq_1, StreamSeq(1));
    assert_eq!(taker_seq_2, StreamSeq(2));
    assert_eq!(
        taker_filled,
        Event::Filled {
            account_id: AccountId(2),
            order_id: OrderId(1),
            side: Side::Buy,
            price: Price(100),
            qty: Qty(5),
            resting_qty: Qty(0),
        }
    );
    assert_eq!(
        taker_accept,
        Event::Accepted {
            account_id: AccountId(2),
            order_id: OrderId(1),
            resting_qty: Qty(0),
        }
    );

    // Maker, on its own connection, sees its own Filled as a second,
    // independently sequenced frame.
    let (maker_seq_2, maker_filled) = read_one_event(&mut maker);
    assert_eq!(maker_seq_2, StreamSeq(2));
    assert_eq!(
        maker_filled,
        Event::Filled {
            account_id: AccountId(1),
            order_id: OrderId(1),
            side: Side::Sell,
            price: Price(100),
            qty: Qty(5),
            resting_qty: Qty(0),
        }
    );
}

#[test]
fn malformed_input_is_rejected_without_reaching_the_matching_thread() {
    let path = spawn_gateway();
    let mut client = connect(&path);

    // A well-formed NewOrder with qty encoded as zero -- decode_command
    // rejects this before it ever becomes a Command (SPEC §3).
    let mut buf = [0u8; wire::MAX_MESSAGE_LEN];
    let len = wire::encode_command(
        &Command::NewOrder {
            account_id: AccountId(9),
            order_id: OrderId(9),
            side: Side::Buy,
            price: Price(100),
            qty: Qty(5),
            kind: OrderKind::Limit,
            tif: Tif::Gtc,
            client_ts: 0,
        },
        &mut buf,
    );
    buf[26..34].copy_from_slice(&0u64.to_le_bytes()); // qty offset -> 0
    client.write_all(&buf[..len]).unwrap();

    let (_seq, event) = read_one_event(&mut client);
    assert_eq!(
        event,
        Event::Rejected {
            account_id: AccountId(0),
            order_id: OrderId(0),
            reason: core::error::RejectReason::ZeroQuantity,
        }
    );

    // The connection is still alive and normal orders still work --
    // rejecting one malformed frame didn't take down the reader.
    send_new_order(&mut client, 1, 1, 100, 5);
    let (_seq, event) = read_one_event(&mut client);
    assert_eq!(
        event,
        Event::Accepted {
            account_id: AccountId(1),
            order_id: OrderId(1),
            resting_qty: Qty(5),
        }
    );
}

/// Gated variant (SPEC §6, PLAN.md stage 6 item 6): proves priority is
/// preserved once arrival order is known. `N` real threads, each owning
/// its own real socket connection, turn-gated by a shared counter so each
/// thread's send-then-await-`Accepted` happens in a known order -- the
/// gate controls *when* each thread is allowed to send, not the socket
/// I/O itself, which is real throughout.
///
/// Verification deliberately avoids racing N reader threads against each
/// other to observe "which maker's fill arrived first": that would only
/// measure which OS thread got scheduled first to acquire a shared lock,
/// not the dispatcher's actual write order, and is exactly the kind of
/// flaky signal this test must not depend on. Instead, each maker offers
/// a *distinct* quantity (1, 2, 3, ..., N), so the sequence of `qty`
/// values in the taker's own `Filled` events -- read from a single
/// connection, in guaranteed program order, no cross-socket racing at all
/// -- directly encodes which maker was consumed at each step of the
/// sweep.
#[test]
fn concurrent_submissions_are_matched_in_strict_arrival_order() {
    const N: usize = 5;
    let path = spawn_gateway();
    let turn = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..N)
        .map(|i| {
            let path = path.clone();
            let turn = Arc::clone(&turn);
            std::thread::spawn(move || {
                let mut conn = connect(&path);
                while turn.load(Ordering::Acquire) != i {
                    std::hint::spin_loop();
                }
                let qty = i as u64 + 1;
                send_new_order_side(&mut conn, 100 + i as u64, 1, Side::Sell, 100, qty);
                let (_seq, event) = read_one_event(&mut conn);
                assert!(
                    matches!(event, Event::Accepted { resting_qty, .. } if resting_qty.0 == qty)
                );
                turn.store(i + 1, Ordering::Release);
                conn
            })
        })
        .collect();
    // Keep the maker connections alive (dropping would close the socket
    // before the dispatcher writes their Filled event) but nothing further
    // needs to be read from them for this test.
    let _maker_conns: Vec<UnixStream> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // All N makers rested in the order 0..N, each with its own distinct
    // quantity (proven above via the turn gate + Accepted resting_qty). A
    // single taker now crosses all of them at once.
    let total_qty: u64 = (1..=N as u64).sum();
    let mut taker = connect(&path);
    send_new_order_side(&mut taker, 999, 1, Side::Buy, 100, total_qty);

    let mut fill_qtys = Vec::with_capacity(N);
    for _ in 0..N {
        let (_seq, event) = read_one_event(&mut taker);
        match event {
            Event::Filled { qty, .. } => fill_qtys.push(qty.0),
            other => panic!("expected a Filled event, got {other:?}"),
        }
    }
    let (_seq, event) = read_one_event(&mut taker);
    assert!(matches!(event, Event::Accepted { resting_qty, .. } if resting_qty.0 == 0));

    let expected: Vec<u64> = (1..=N as u64).collect();
    assert_eq!(
        fill_qtys,
        expected,
        "fills must reflect strict rest arrival order (maker 0's qty=1 consumed first, ..., maker {}'s qty={N} consumed last), got {fill_qtys:?}",
        N - 1
    );
}

/// Ungated variant: genuinely uncoordinated concurrent submission -- N
/// real threads, N real connections, all released together with no
/// ordering between them. Since arrival order is neither known nor
/// controlled here, only order-independent properties are asserted: no
/// double-fill / fills conserved per account, every order eventually
/// fully fills (guaranteed since total buy volume equals total sell
/// volume at one price, regardless of arrival order), and the book is
/// never observed crossed.
#[test]
fn concurrent_submissions_with_no_ordering_gate_preserve_book_invariants() {
    const PAIRS: usize = 5;
    let (path, market_data_rx) = spawn_gateway_with_market_data();

    let crossed = Arc::new(Mutex::new(Vec::new()));
    {
        let crossed = Arc::clone(&crossed);
        std::thread::spawn(move || {
            for event in market_data_rx {
                if let Event::BookUpdate {
                    best_bid: Some((bid, _)),
                    best_ask: Some((ask, _)),
                } = event
                    && bid.0 >= ask.0
                {
                    crossed.lock().unwrap().push((bid, ask));
                }
            }
        });
    }

    let barrier = Arc::new(Barrier::new(PAIRS * 2));
    let handles: Vec<_> = (0..PAIRS * 2)
        .map(|i| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            let side = if i < PAIRS { Side::Buy } else { Side::Sell };
            let account = 200 + i as u64;
            std::thread::spawn(move || {
                let mut conn = connect(&path);
                barrier.wait();
                send_new_order_side(&mut conn, account, 1, side, 100, 1);

                let mut filled = 0u64;
                let mut resting = 1u64;
                while resting > 0 {
                    let (_seq, event) = read_one_event(&mut conn);
                    match event {
                        Event::Accepted { resting_qty, .. } => resting = resting_qty.0,
                        Event::Filled { qty, resting_qty, .. } => {
                            filled += qty.0;
                            resting = resting_qty.0;
                        }
                        other => panic!("unexpected event for a simple {side:?} order: {other:?}"),
                    }
                    assert_eq!(
                        filled + resting,
                        1,
                        "no double-fill: filled + resting must always equal submitted qty"
                    );
                }
                assert_eq!(
                    resting, 0,
                    "with equal buy and sell volume at one price, every order must eventually fully fill"
                );
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    // Give the market-data drain a brief moment to catch up, then check
    // whether it ever observed a crossed book.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        crossed.lock().unwrap().as_slice(),
        &[],
        "book must never be observed crossed, even under fully uncoordinated concurrent submission"
    );
}
