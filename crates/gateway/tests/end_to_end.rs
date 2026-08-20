//! Stage 3 exit criterion (PLAN.md): an order sent over the socket
//! produces a correct execution report, and a second connection is served
//! independently.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
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
        )
    });

    path
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
