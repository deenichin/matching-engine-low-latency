//! Accepts market-data subscribers and fans events out to them (SPEC §4,
//! §8).
//!
//! Mirrors `gateway`'s thread-per-connection shape (this crate cannot
//! depend on `gateway` — SPEC §4 scopes `Transport` to the ingress/DPDK
//! story specifically, and marketdata depends on `wire`, `core` only) but
//! the backpressure story is the opposite of order entry's: order entry's
//! return channel blocks when full (a client is expected to keep reading
//! its own responses); market data's per-subscriber queues never block a
//! push, they drop the oldest item instead (SPEC §8). That difference is
//! the whole reason the two are separate sockets on separate threads —
//! nothing here can ever stall the matching thread or order entry.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use core::event::Event;
use core::types::StreamSeq;

use crate::queue::DropOldestQueue;

/// Identifies one connected market-data subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriberId(pub u64);

/// Per-subscriber queue capacity — bounded, per SPEC §8's drop-oldest
/// policy. A heuristic starting point, the same spirit as the
/// connection-routing table's capacity in `gateway` (stage 3): real
/// sizing is stage 7 benchmarking work, not a stage 5 one.
const SUBSCRIBER_QUEUE_CAPACITY: usize = 1024;

type SubscriberMap = Arc<Mutex<HashMap<SubscriberId, Arc<DropOldestQueue>>>>;

/// Binds the market-data socket, unlinking any stale file first (SPEC §3).
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    UnixListener::bind(path)
}

/// Runs the market-data side on the calling thread: accepts subscriber
/// connections and spawns a writer thread per subscriber, each with its
/// own drop-oldest queue. Also spawns one internal fan-out thread that
/// drains `market_data_rx` (fed by the matching thread) and pushes each
/// event into every currently-connected subscriber's own queue —
/// `DropOldestQueue::push` never blocks, so a slow subscriber can never
/// stall this fan-out, and this fan-out never touches the matching
/// thread's own channel send (`try_send`, stage 3) or order entry's
/// sockets at all.
///
/// Blocks on the accept loop until `listener` stops accepting.
pub fn run_market_data(listener: UnixListener, market_data_rx: Receiver<Event>) {
    let subscribers: SubscriberMap = Arc::new(Mutex::new(HashMap::new()));

    {
        let subscribers = Arc::clone(&subscribers);
        std::thread::spawn(move || run_fan_out(market_data_rx, subscribers));
    }

    let mut next_id: u64 = 1;
    loop {
        let Ok((stream, _addr)) = listener.accept() else {
            break;
        };
        let id = SubscriberId(next_id);
        next_id += 1;

        let queue = Arc::new(DropOldestQueue::new(SUBSCRIBER_QUEUE_CAPACITY));
        subscribers
            .lock()
            .expect("subscribers mutex poisoned")
            .insert(id, Arc::clone(&queue));

        let subscribers_for_writer = Arc::clone(&subscribers);
        std::thread::spawn(move || {
            run_subscriber_writer(id, stream, queue, subscribers_for_writer)
        });
    }
}

/// Drains `market_data_rx` and copies each event into every subscriber's
/// own queue. `Event` is small and `Copy` (SPEC §4) — this is a cheap
/// broadcast, not a serialize-and-fan-out.
fn run_fan_out(market_data_rx: Receiver<Event>, subscribers: SubscriberMap) {
    for event in market_data_rx {
        let subs = subscribers.lock().expect("subscribers mutex poisoned");
        for queue in subs.values() {
            queue.push(event);
        }
    }
}

/// One subscriber's write side: blocks on its own queue, encodes with its
/// own `StreamSeq` — independent of every other subscriber's, of
/// execution reports', and of `EngineSeq` — and writes to its own socket.
/// The seq jumps by `1 + dropped` on each pop (SPEC §8: "detects the loss
/// via a sequence-number gap") rather than counting only what this
/// subscriber actually received, which would hide every drop.
fn run_subscriber_writer(
    id: SubscriberId,
    mut stream: UnixStream,
    queue: Arc<DropOldestQueue>,
    subscribers: SubscriberMap,
) {
    let mut seq: u64 = 0;
    loop {
        let (event, dropped) = queue.pop_blocking();
        seq += 1 + dropped;

        let mut buf = [0u8; wire::MAX_MESSAGE_LEN];
        let len = wire::encode_event(StreamSeq(seq), &event, &mut buf);
        if stream.write_all(&buf[..len]).is_err() {
            subscribers
                .lock()
                .expect("subscribers mutex poisoned")
                .remove(&id);
            break;
        }
    }
}
