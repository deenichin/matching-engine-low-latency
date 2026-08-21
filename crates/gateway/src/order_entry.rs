//! Order-entry connection handling: the accept loop, per-connection
//! framing/decode/validation, and the write side that stamps `stream_seq`
//! and writes execution reports back (SPEC §3, §4).
//!
//! SPEC §4 describes one "gateway thread" owning both the read and write
//! sides of the order-entry socket. Multiplexing many blocking UDS
//! connections on a single OS thread needs readiness polling (`epoll`)
//! that's either `unsafe` FFI or a new dependency — both raised with the
//! user rather than added silently. What's built instead: the accept loop
//! runs on the calling thread, each accepted connection gets its own
//! reader thread (blocking read → frame → decode → push to the command
//! channel), and one shared return-dispatcher thread owns every
//! connection's write half and drains the return channel. Std-only, no
//! `unsafe`. The three-thread *model* SPEC §4 cares about — gateway vs.
//! matching vs. market-data as separate concerns, so a slow subscriber or
//! a slow reader never stalls the matching thread — still holds: the
//! matching thread only ever touches bounded channels, never a socket.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};

use core::error::RejectReason;
use core::event::{Command, Event};
use core::types::{AccountId, EngineSeq, OrderId, StreamSeq};
use wire::Framer;

/// Sentinel for a `Rejected` event that never became a `Command` at all
/// -- a malformed frame or unknown tag, rejected by this connection's
/// reader before `wire::decode_command` ever succeeds, so it never
/// reaches `risk::process_command` and has no real `EngineSeq` to carry
/// (SPEC §2). `EngineSeq` starts at `EngineSeq(1)` for the first command
/// that actually enters the system (`risk::process_command` increments
/// before it reads), so `0` is otherwise never produced and is safe to
/// reserve as "rejected at the wire layer, before this ever became a
/// command" -- clients tracking their own command flow can treat it as
/// "did not consume a sequence number."
const WIRE_LEVEL_REJECT_ENGINE_SEQ: EngineSeq = EngineSeq(0);

use crate::conn::ConnId;
use crate::transport::{Transport, UdsTransport};

/// A read from a connection's socket, sized generously against
/// [`wire::MAX_MESSAGE_LEN`] so a single `read()` commonly delivers
/// several frames at once (SPEC §3) without forcing this to be tuned
/// per-message-type.
const READ_BUF_LEN: usize = 4096;

/// Runs the order-entry side on the calling thread: accepts connections,
/// assigns each a [`ConnId`], and spawns the per-connection reader thread
/// and the shared return-dispatcher thread described in the module docs.
///
/// Blocks until `transport`'s `accept()` starts erroring — which is what
/// happens once the transport (and its underlying listener) is dropped
/// from another thread, the graceful-shutdown path `bin` uses.
pub fn run_order_entry(
    transport: UdsTransport,
    command_tx: SyncSender<(ConnId, Command)>,
    return_tx: SyncSender<(ConnId, EngineSeq, Event)>,
    return_rx: Receiver<(ConnId, EngineSeq, Event)>,
) {
    let writers: Arc<Mutex<HashMap<ConnId, UnixStream>>> = Arc::new(Mutex::new(HashMap::new()));

    {
        let writers = Arc::clone(&writers);
        std::thread::spawn(move || run_return_dispatcher(return_rx, writers));
    }

    let mut next_conn_id: u64 = 1;
    loop {
        let stream = match transport.accept() {
            Ok(stream) => stream,
            Err(_) => break,
        };
        let conn_id = ConnId(next_conn_id);
        next_conn_id += 1;

        let Ok(write_half) = stream.try_clone() else {
            continue; // couldn't split this connection; drop it, keep serving others
        };
        writers
            .lock()
            .expect("writers mutex poisoned")
            .insert(conn_id, write_half);

        let command_tx = command_tx.clone();
        let return_tx = return_tx.clone();
        std::thread::spawn(move || run_reader(conn_id, stream, command_tx, return_tx));
    }
}

/// One connection's read side: frames and decodes inbound bytes, pushing
/// valid commands onto `command_tx`. Input that fails wire-level
/// validation is rejected directly onto `return_tx` — it never reaches
/// `command_tx`, and so never reaches `Engine::apply` (SPEC §3).
fn run_reader(
    conn_id: ConnId,
    mut stream: UnixStream,
    command_tx: SyncSender<(ConnId, Command)>,
    return_tx: SyncSender<(ConnId, EngineSeq, Event)>,
) {
    let mut framer = Framer::new();
    let mut read_buf = [0u8; READ_BUF_LEN];

    loop {
        let n = match stream.read(&mut read_buf) {
            Ok(0) => break, // connection closed
            Ok(n) => n,
            Err(_) => break,
        };
        framer.feed(&read_buf[..n]);

        let drain_result = framer.drain_frames(|frame| match wire::decode_command(frame) {
            Ok(cmd) => {
                let _ = command_tx.send((conn_id, cmd));
            }
            Err(reason) => {
                let _ = return_tx.send((
                    conn_id,
                    WIRE_LEVEL_REJECT_ENGINE_SEQ,
                    malformed_reject(reason),
                ));
            }
        });
        if drain_result.is_err() {
            // An unrecognized tag desyncs framing -- there is no length to
            // skip past to find the next frame, so this connection's
            // stream is unrecoverable from here.
            let _ = return_tx.send((
                conn_id,
                WIRE_LEVEL_REJECT_ENGINE_SEQ,
                malformed_reject(RejectReason::UnknownMessageType),
            ));
            break;
        }
    }
}

/// Malformed input can't reliably be attributed to a specific order or
/// account — `AccountId(0)`/`OrderId(0)` mark that explicitly rather than
/// guessing. Not a SPEC-mandated sentinel; SPEC does not name a value for
/// this case.
fn malformed_reject(reason: RejectReason) -> Event {
    Event::Rejected {
        account_id: AccountId(0),
        order_id: OrderId(0),
        reason,
    }
}

/// The shared write side: drains `return_rx` and, for each event, stamps
/// it with *that connection's own* `StreamSeq` — execution reports are a
/// per-connection stream (SPEC §2, §3: "request/response per
/// connection"), not one counter shared across every client — encodes it,
/// and writes it to the originating connection. A write failure (the
/// client went away) drops that connection's entry; nothing tries to
/// operate on a closed connection again after that.
fn run_return_dispatcher(
    return_rx: Receiver<(ConnId, EngineSeq, Event)>,
    writers: Arc<Mutex<HashMap<ConnId, UnixStream>>>,
) {
    let mut seqs: HashMap<ConnId, u64> = HashMap::new();

    for (conn_id, engine_seq, event) in return_rx {
        let seq = seqs.entry(conn_id).or_insert(0);
        *seq += 1;

        let mut buf = [0u8; wire::MAX_MESSAGE_LEN];
        let len = wire::encode_event(StreamSeq(*seq), Some(engine_seq), &event, &mut buf);

        let mut writers = writers.lock().expect("writers mutex poisoned");
        let wrote_ok = writers
            .get_mut(&conn_id)
            .is_some_and(|stream| stream.write_all(&buf[..len]).is_ok());
        if !wrote_ok {
            writers.remove(&conn_id);
            seqs.remove(&conn_id);
        }
    }
}
