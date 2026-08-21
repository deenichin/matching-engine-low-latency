//! The matching thread (SPEC §4): busy-spins on the command channel, is
//! the sole caller of `Engine::apply`, and dispatches whatever `Event`s
//! result individually — never batched into a `Vec<Event>`. A `Vec<Event>`
//! per dispatch would allocate on every submit, cancel, and modify, which
//! defeats the zero-allocation hot-path claim silently — nothing in stage
//! 1's tests would catch it, since they never touch a channel. If a fill
//! produces three events, that's three `send` calls, not one send of
//! three.
//!
//! This thread never computes a `stream_seq` and never sees a `ConnId`
//! except to pass it through unopened: it reads `(ConnId, Command)`,
//! writes `(ConnId, Event)`. Sequence stamping happens downstream, in the
//! gateway thread's return dispatcher, at the point it calls
//! `wire::encode_event` — not here, and not in `Engine`.
//!
//! **Routing is not "reply to whoever sent the command."** A single
//! command can produce events for participants other than its own
//! sender: a taker's submit fills a resting maker on a *different*
//! connection, self-trade prevention cancels a resting maker on a
//! *different* connection, and `MassCancel` cancels orders that may have
//! rested from *several different* connections over time. This thread
//! keeps a `(AccountId, OrderId) -> ConnId` table, updated as orders rest
//! and stop resting, and routes each event by looking up *its own*
//! account/order — not by assuming it belongs to the command that
//! triggered it.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};

use core::types::{AccountId, EngineSeq, MAX_OPEN_ORDERS, OrderId};
use core::{Command, Engine, Event};

use crate::conn::ConnId;

/// Rough working-set size for the connection-routing table's starting
/// capacity — not a real account count, just an order-of-magnitude guess
/// for a reference deployment. Purely a sizing heuristic.
const EXPECTED_CONCURRENT_ACCOUNTS: usize = 64;

/// Starting capacity for `resting_conn`, so the table doesn't
/// reallocate-and-rehash repeatedly while ramping up from empty: expected
/// accounts times the most orders any one of them can have resting at
/// once (`MAX_OPEN_ORDERS`, SPEC §5). This is a heuristic, not a hard
/// bound — exceeding it is not a correctness problem, the table still
/// grows like any `HashMap`, only the amortization is missed past this
/// point. A real allocations-per-1,000-operations figure for this table
/// is stage 7's `BENCH.md` work, the same measurement discipline already
/// applied to the price map's B-tree node splits (SPEC §4).
const RESTING_CONN_INITIAL_CAPACITY: usize = EXPECTED_CONCURRENT_ACCOUNTS * MAX_OPEN_ORDERS;

/// Which connection a command's own account/order is (`NewOrder`,
/// `CancelOrder`, `CancelReplace`) — `None` for commands with no single
/// target order (`MassCancel`, whose `Cancelled` events must each be
/// routed by their own account/order instead; `KillSwitch`/`Snapshot`,
/// which produce no events yet).
fn command_self_id(cmd: &Command) -> Option<(AccountId, OrderId)> {
    match cmd {
        Command::NewOrder {
            account_id,
            order_id,
            ..
        }
        | Command::CancelOrder {
            account_id,
            order_id,
        }
        | Command::CancelReplace {
            account_id,
            order_id,
            ..
        } => Some((*account_id, *order_id)),
        Command::MassCancel { .. } | Command::KillSwitch { .. } | Command::Snapshot => None,
    }
}

/// The account/order an event is about. `None` for market-data events,
/// which carry no account identity by design (SPEC §2).
fn event_id(event: &Event) -> Option<(AccountId, OrderId)> {
    match event {
        Event::Accepted {
            account_id,
            order_id,
            ..
        }
        | Event::Rejected {
            account_id,
            order_id,
            ..
        }
        | Event::Filled {
            account_id,
            order_id,
            ..
        }
        | Event::Cancelled {
            account_id,
            order_id,
        }
        | Event::Replaced {
            account_id,
            order_id,
            ..
        } => Some((*account_id, *order_id)),
        Event::Trade { .. } | Event::BookUpdate { .. } => None,
        // Snapshot responses are routed directly to the requesting
        // connection before this function is ever consulted (see
        // `run_matching_thread`'s dedicated routing arm) -- they carry
        // no single (account, order) identity to route by anyway.
        Event::SnapshotLevel { .. }
        | Event::SnapshotAccount { .. }
        | Event::SnapshotSummary { .. } => None,
    }
}

/// Environment variable that gates CPU pinning, read once at matching-
/// thread startup. Defaults to on; `MATCHING_PIN=0` skips it. Exists so
/// the pinned-vs-unpinned HDR comparison in BENCH.md (stage 9) is
/// reproducible from one binary with two commands, rather than requiring
/// two separately-built binaries that nobody could rebuild identically
/// afterward.
const MATCHING_PIN_ENV_VAR: &str = "MATCHING_PIN";

/// Pins the calling thread to a dedicated CPU core (SPEC §10, stage 9), so
/// the OS scheduler cannot migrate or preempt it mid-spin -- either of
/// which would reintroduce the jitter busy-spinning (above) exists to
/// remove. Must be called from the matching thread itself:
/// `core_affinity::set_for_current` only ever affects the calling thread.
///
/// Uses `core_affinity` rather than a raw `sched_setaffinity` FFI call --
/// CLAUDE.md permits `unsafe` only at FFI/hardware boundaries, naming CPU
/// affinity as the one candidate, and `core_affinity` wraps the platform
/// call behind a safe API, so no `unsafe` is needed here at all.
///
/// Pins to the *first* core id the OS reports as available to this
/// process -- taken, not chosen for any property of that core. Inside the
/// `engine`/`dev` containers that's the lowest id within whatever range
/// `docker-compose.yml`'s `cpuset` grants the container, not necessarily
/// physical core 0. `sched_setaffinity`'s real, enforced effect is
/// Linux-only: on the bare macOS host this call does not panic or error,
/// but the affinity is at best an unenforced scheduling hint (see
/// BENCH.md for what was actually measured).
///
/// Never panics on failure (no core ids reported, or the OS rejects the
/// affinity mask) -- pinning is a latency optimization, not a correctness
/// requirement, so a failed pin must not prevent the matching thread from
/// running at all. Also runs whenever `matching`'s own unit tests spawn
/// `run_matching_thread` (below); harmless, since multiple threads can
/// share a core, if a little more contended during `cargo test`.
fn pin_to_dedicated_core() {
    if std::env::var(MATCHING_PIN_ENV_VAR).as_deref() == Ok("0") {
        eprintln!("matching-engine: MATCHING_PIN=0; matching thread left unpinned");
        return;
    }
    let Some(core_id) = core_affinity::get_core_ids().and_then(|ids| ids.into_iter().next()) else {
        eprintln!("matching-engine: no CPU core ids reported; matching thread not pinned");
        return;
    };
    if core_affinity::set_for_current(core_id) {
        eprintln!(
            "matching-engine: matching thread pinned to core {}",
            core_id.id
        );
    } else {
        eprintln!(
            "matching-engine: failed to pin matching thread to core {}; continuing unpinned",
            core_id.id
        );
    }
}

/// Runs the matching thread on the calling thread. Returns when
/// `command_rx` disconnects (every sender dropped) — the orderly shutdown
/// path; there is no other exit.
///
/// `risk_state` is checked before every `Engine::apply` call, inside this
/// loop — not at ingress — so a command already sitting in the channel
/// when the kill switch fires is still checked against the new state
/// (SPEC §5). It is `&mut` because a `KillSwitch` command updates it.
///
/// `market_data_tx` routes `Trade` and `BookUpdate` events to the
/// marketdata thread via `try_send` (never blocking — a slow or absent
/// subscriber side must never stall matching, SPEC §8), while every other
/// event goes to `return_tx` addressed by the connection its own
/// account/order last rested from.
///
/// `recorder`, when `Some`, is stage 6's recording hook (SPEC §7): each
/// command is written here — post-gateway-validation, before risk, exactly
/// as SPEC specifies — via the same self-framing `wire::encode_command`
/// format live traffic already uses on the wire, so replay can read it back
/// with the existing `Framer`/`decode_command` rather than a parallel
/// format. This is deliberately opt-in I/O on the matching thread: it is
/// not part of the zero-allocation hot-path guarantee, which only applies
/// when recording is off (the default).
pub fn run_matching_thread(
    command_rx: Receiver<(ConnId, Command)>,
    return_tx: SyncSender<(ConnId, EngineSeq, Event)>,
    market_data_tx: SyncSender<Event>,
    mut risk_state: risk::RiskState,
    mut recorder: Option<BufWriter<File>>,
) {
    pin_to_dedicated_core();

    let mut engine = Engine::new();
    let mut resting_conn: HashMap<(AccountId, OrderId), ConnId> =
        HashMap::with_capacity(RESTING_CONN_INITIAL_CAPACITY);
    // Every command this thread ever processes gets exactly one
    // `EngineSeq`, assigned inside `risk::process_command` itself (the
    // one increment site, SPEC §2) -- this counter lives for the
    // thread's whole lifetime, mirroring how `resting_conn` does.
    let mut engine_seq = EngineSeq(0);

    loop {
        let (conn_id, cmd) = match command_rx.try_recv() {
            Ok(item) => item,
            Err(TryRecvError::Empty) => {
                std::hint::spin_loop();
                continue;
            }
            Err(TryRecvError::Disconnected) => break,
        };

        if let Some(writer) = recorder.as_mut() {
            let mut buf = [0u8; wire::MAX_MESSAGE_LEN];
            let len = wire::encode_command(&cmd, &mut buf);
            // Recording is a best-effort diagnostic feature, not part of
            // the reliability contract of order processing itself -- a
            // write failure here must never stop a command from being
            // matched.
            let _ = writer.write_all(&buf[..len]);
        }

        let self_id = command_self_id(&cmd);

        risk::process_command(
            &mut engine,
            &mut risk_state,
            cmd,
            &mut engine_seq,
            &mut |seq, event| {
                if matches!(event, Event::Trade { .. } | Event::BookUpdate { .. }) {
                    let _ = market_data_tx.try_send(event);
                    return;
                }

                // A snapshot's response always belongs to whoever asked for
                // it, never a counterparty -- unlike fills/STP/mass-cancel,
                // which can legitimately target a different connection's
                // resting order. Routed directly, bypassing `resting_conn`
                // entirely, and carrying no `EngineSeq` on the wire (SPEC
                // §2: `Snapshot*` events are a point-in-time dump, not tied
                // to any one command).
                if matches!(
                    event,
                    Event::SnapshotLevel { .. }
                        | Event::SnapshotAccount { .. }
                        | Event::SnapshotSummary { .. }
                ) {
                    let _ = return_tx.send((conn_id, seq, event));
                    return;
                }

                // Always Some here -- every non-market-data, non-snapshot
                // Event variant carries an account/order (checked above).
                let id =
                    event_id(&event).expect("execution-report events always carry account/order");
                let route = if Some(id) == self_id {
                    conn_id
                } else {
                    // A counterparty touched incidentally by this command --
                    // STP, or a fill/mass-cancel against an order that rested
                    // from a different connection. Falling back to conn_id
                    // would silently misroute to the wrong client if the
                    // table is ever missing an entry it shouldn't be.
                    *resting_conn.get(&id).unwrap_or(&conn_id)
                };

                match &event {
                    Event::Accepted { resting_qty, .. } | Event::Filled { resting_qty, .. }
                        if resting_qty.0 > 0 =>
                    {
                        resting_conn.insert(id, route);
                    }
                    Event::Filled { resting_qty, .. } if resting_qty.0 == 0 => {
                        resting_conn.remove(&id);
                    }
                    Event::Cancelled { .. } => {
                        resting_conn.remove(&id);
                    }
                    Event::Replaced { .. } => {
                        // Still resting somewhere after the amend (a modify
                        // that empties out ends in Filled{resting_qty: 0}
                        // instead, handled above) -- refresh the association
                        // in case this modify came from a different
                        // connection than the original submit (SPEC §4: an
                        // account may hold more than one open connection).
                        resting_conn.insert(id, route);
                    }
                    _ => {}
                }

                let _ = return_tx.send((route, seq, event));
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::types::{FillState, OrderKind, Price, Qty, Side, Tif};
    use std::sync::mpsc;
    use std::time::Duration;

    type CommandSender = mpsc::SyncSender<(ConnId, Command)>;
    type ReturnReceiver = mpsc::Receiver<(ConnId, EngineSeq, Event)>;

    fn spawn() -> (CommandSender, ReturnReceiver) {
        let (command_tx, command_rx) = mpsc::sync_channel(16);
        let (return_tx, return_rx) = mpsc::sync_channel(16);
        let (market_data_tx, _market_data_rx) = mpsc::sync_channel::<Event>(16);
        let risk_state = risk::RiskState::new(risk::RiskConfig::default());
        std::thread::spawn(move || {
            run_matching_thread(command_rx, return_tx, market_data_tx, risk_state, None)
        });
        (command_tx, return_rx)
    }

    fn recv(return_rx: &mpsc::Receiver<(ConnId, EngineSeq, Event)>) -> (ConnId, EngineSeq, Event) {
        return_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("matching thread did not respond in time")
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
    fn fill_routes_maker_and_taker_events_to_their_own_connections() {
        let (command_tx, return_rx) = spawn();
        let maker_conn = ConnId(1);
        let taker_conn = ConnId(2);

        command_tx
            .send((maker_conn, new_order(1, 1, Side::Sell, 100, 5)))
            .unwrap();
        let (conn, _seq, event) = recv(&return_rx);
        assert_eq!(conn, maker_conn);
        assert_eq!(
            event,
            Event::Accepted {
                account_id: AccountId(1),
                order_id: OrderId(1),
                resting_qty: Qty(5)
            }
        );

        command_tx
            .send((taker_conn, new_order(2, 1, Side::Buy, 100, 5)))
            .unwrap();

        let (conn_a, _seq_a, event_a) = recv(&return_rx);
        let (conn_b, _seq_b, event_b) = recv(&return_rx);
        let (conn_c, _seq_c, event_c) = recv(&return_rx);

        // Taker's own Filled and Accepted route to the taker's connection;
        // the maker's Filled -- for a completely different account --
        // routes to the maker's connection, not the taker's.
        assert_eq!(conn_a, taker_conn);
        assert_eq!(
            event_a,
            Event::Filled {
                account_id: AccountId(2),
                order_id: OrderId(1),
                side: Side::Buy,
                price: Price(100),
                qty: Qty(5),
                resting_qty: Qty(0),
                state: FillState::Filled,
            }
        );
        assert_eq!(conn_b, maker_conn);
        assert_eq!(
            event_b,
            Event::Filled {
                account_id: AccountId(1),
                order_id: OrderId(1),
                side: Side::Sell,
                price: Price(100),
                qty: Qty(5),
                resting_qty: Qty(0),
                state: FillState::Filled,
            }
        );
        assert_eq!(conn_c, taker_conn);
        assert_eq!(
            event_c,
            Event::Accepted {
                account_id: AccountId(2),
                order_id: OrderId(1),
                resting_qty: Qty(0),
            }
        );
    }

    #[test]
    fn stp_cancellation_routes_to_the_cancelled_makers_own_connection() {
        let (command_tx, return_rx) = spawn();
        // Same account (1), two different connections -- e.g. two
        // sessions for the same account (SPEC §4).
        let resting_conn_id = ConnId(10);
        let aggressor_conn_id = ConnId(20);

        command_tx
            .send((resting_conn_id, new_order(1, 1, Side::Sell, 100, 5)))
            .unwrap();
        let (conn, _seq, _accepted) = recv(&return_rx);
        assert_eq!(conn, resting_conn_id);

        // Same account crosses its own resting order from a different
        // connection -- STP cancels it.
        command_tx
            .send((aggressor_conn_id, new_order(1, 2, Side::Buy, 100, 3)))
            .unwrap();

        let (conn_a, _seq_a, event_a) = recv(&return_rx);
        let (conn_b, _seq_b, event_b) = recv(&return_rx);

        assert_eq!(
            conn_a, resting_conn_id,
            "STP cancel must route to the maker's own connection"
        );
        assert_eq!(
            event_a,
            Event::Cancelled {
                account_id: AccountId(1),
                order_id: OrderId(1),
            }
        );
        assert_eq!(conn_b, aggressor_conn_id);
        assert_eq!(
            event_b,
            Event::Accepted {
                account_id: AccountId(1),
                order_id: OrderId(2),
                resting_qty: Qty(3),
            }
        );
    }

    #[test]
    fn mass_cancel_routes_each_order_to_its_own_originating_connection() {
        let (command_tx, return_rx) = spawn();
        let conn_a = ConnId(100);
        let conn_b = ConnId(200);
        let issuer_conn = ConnId(300);

        // Same account, two orders rested from two different connections.
        command_tx
            .send((conn_a, new_order(5, 1, Side::Sell, 100, 1)))
            .unwrap();
        recv(&return_rx);
        command_tx
            .send((conn_b, new_order(5, 2, Side::Sell, 101, 1)))
            .unwrap();
        recv(&return_rx);

        // Mass-cancel issued from yet a third connection.
        command_tx
            .send((
                issuer_conn,
                Command::MassCancel {
                    account_id: AccountId(5),
                },
            ))
            .unwrap();

        let (first_conn, _first_seq, first_event) = recv(&return_rx);
        let (second_conn, _second_seq, second_event) = recv(&return_rx);

        let mut by_order: HashMap<OrderId, ConnId> = HashMap::new();
        for (conn, event) in [(first_conn, first_event), (second_conn, second_event)] {
            match event {
                Event::Cancelled { order_id, .. } => {
                    by_order.insert(order_id, conn);
                }
                other => panic!("expected Cancelled, got {other:?}"),
            }
        }

        assert_eq!(by_order.get(&OrderId(1)), Some(&conn_a));
        assert_eq!(by_order.get(&OrderId(2)), Some(&conn_b));
        // Neither cancellation goes to the connection that issued the
        // mass-cancel -- each routes back to where its own order rested.
        assert!(!by_order.values().any(|&c| c == issuer_conn));
    }
}
