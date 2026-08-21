//! The single entry point the matching thread calls (SPEC §4:
//! `Engine::apply`). `Engine` knows nothing about sockets, connections, or
//! sequence numbers — it only ever sees `Command` in, `Event` out.

use crate::book::Book;
use crate::event::{Command, Event};
use crate::types::{OrderKind, Tif};

/// Wraps `Book` behind the one call site SPEC §4 names: `Engine::apply`.
///
/// `KillSwitch` and `Snapshot` both travel the same channel as every other
/// `Command` (so `apply` stays total over every variant), but only
/// `Snapshot` does anything here: it dumps current book state
/// (`Book::snapshot`). `KillSwitch` is a no-op in `Engine` — its *state*
/// belongs to `risk` (SPEC §4), checked by the matching thread *before*
/// `apply` is ever called, so there is nothing left for `Engine` itself to
/// act on.
#[derive(Debug, Default)]
pub struct Engine {
    book: Book,
}

impl Engine {
    pub fn new() -> Self {
        Self { book: Book::new() }
    }

    /// Read access to the book, for callers that need to inspect state
    /// directly (`assert_invariants()` in tests, a future `Snapshot`
    /// implementation). Not part of the hot path.
    pub fn book(&self) -> &Book {
        &self.book
    }

    /// Apply one command, emitting whatever `Event`s result. The sole
    /// caller is the matching thread; nothing about `ConnId`, sockets, or
    /// stream sequencing reaches this function or anything it calls.
    ///
    /// Emits `BookUpdate` last, at most once, iff this command changed the
    /// top of book (SPEC §8: "on every book change" — the touch, not
    /// every mutation deep in the book). A snapshot before and after the
    /// whole dispatch, compared once, rather than scattering the check
    /// across every `Book` method that could move the touch.
    pub fn apply(&mut self, cmd: Command, emit: &mut dyn FnMut(Event)) {
        let before = self.book.top_of_book();
        self.dispatch(cmd, emit);
        let after = self.book.top_of_book();
        if before != after {
            emit(Event::BookUpdate {
                best_bid: after.0,
                best_ask: after.1,
            });
        }
    }

    fn dispatch(&mut self, cmd: Command, emit: &mut dyn FnMut(Event)) {
        match cmd {
            Command::NewOrder {
                account_id,
                order_id,
                side,
                price,
                qty,
                kind,
                tif,
                client_ts: _,
            } => match kind {
                OrderKind::Market => self
                    .book
                    .submit_market(account_id, order_id, side, qty, emit),
                OrderKind::Limit => match tif {
                    Tif::Gtc => self
                        .book
                        .submit_gtc(account_id, order_id, side, price, qty, emit),
                    Tif::Ioc => self
                        .book
                        .submit_ioc(account_id, order_id, side, price, qty, emit),
                    Tif::Fok => self
                        .book
                        .submit_fok(account_id, order_id, side, price, qty, emit),
                    Tif::PostOnly => self
                        .book
                        .submit_postonly(account_id, order_id, side, price, qty, emit),
                },
            },
            Command::CancelOrder {
                account_id,
                order_id,
            } => self.book.cancel(account_id, order_id, emit),
            Command::CancelReplace {
                account_id,
                order_id,
                new_price,
                new_qty,
            } => self
                .book
                .modify(account_id, order_id, new_price, new_qty, emit),
            Command::MassCancel { account_id } => self.book.mass_cancel(account_id, emit),
            Command::Snapshot => self.book.snapshot(emit),
            // Kill-switch *state* belongs to risk (SPEC §4), checked
            // before `apply` is ever called -- there is nothing left for
            // `Engine` itself to do with it.
            Command::KillSwitch { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AccountId, OrderId, Price, Qty, Side};

    #[test]
    fn apply_new_order_rests_and_emits_accepted() {
        let mut engine = Engine::new();
        let mut events = Vec::new();
        engine.apply(
            Command::NewOrder {
                account_id: AccountId(1),
                order_id: OrderId(1),
                side: Side::Buy,
                price: Price(100),
                qty: Qty(5),
                kind: OrderKind::Limit,
                tif: Tif::Gtc,
                client_ts: 0,
            },
            &mut |e| events.push(e),
        );
        assert_eq!(
            events,
            vec![
                Event::Accepted {
                    account_id: AccountId(1),
                    order_id: OrderId(1),
                    resting_qty: Qty(5),
                },
                Event::BookUpdate {
                    best_bid: Some((Price(100), Qty(5))),
                    best_ask: None,
                },
            ]
        );
        engine.book().assert_invariants();
        assert_eq!(engine.book().best_bid(), Some(Price(100)));
    }

    #[test]
    fn apply_cancel_removes_the_resting_order() {
        let mut engine = Engine::new();
        engine.apply(
            Command::NewOrder {
                account_id: AccountId(1),
                order_id: OrderId(1),
                side: Side::Buy,
                price: Price(100),
                qty: Qty(5),
                kind: OrderKind::Limit,
                tif: Tif::Gtc,
                client_ts: 0,
            },
            &mut |_| {},
        );

        let mut events = Vec::new();
        engine.apply(
            Command::CancelOrder {
                account_id: AccountId(1),
                order_id: OrderId(1),
            },
            &mut |e| events.push(e),
        );
        assert_eq!(
            events,
            vec![
                Event::Cancelled {
                    account_id: AccountId(1),
                    order_id: OrderId(1),
                },
                Event::BookUpdate {
                    best_bid: None,
                    best_ask: None,
                },
            ]
        );
        engine.book().assert_invariants();
        assert_eq!(engine.book().best_bid(), None);
    }

    #[test]
    fn book_update_is_not_emitted_when_the_touch_is_unchanged() {
        let mut engine = Engine::new();
        // Two resting orders at the same price: the first sets the touch,
        // the second doesn't move it (same price, same aggregate qty
        // shape at the level) -- no BookUpdate for the second submit.
        engine.apply(
            Command::NewOrder {
                account_id: AccountId(1),
                order_id: OrderId(1),
                side: Side::Buy,
                price: Price(100),
                qty: Qty(5),
                kind: OrderKind::Limit,
                tif: Tif::Gtc,
                client_ts: 0,
            },
            &mut |_| {},
        );

        // A second, deeper resting order (worse price) does not move the
        // touch at all.
        let mut events = Vec::new();
        engine.apply(
            Command::NewOrder {
                account_id: AccountId(2),
                order_id: OrderId(2),
                side: Side::Buy,
                price: Price(90),
                qty: Qty(3),
                kind: OrderKind::Limit,
                tif: Tif::Gtc,
                client_ts: 0,
            },
            &mut |e| events.push(e),
        );
        assert_eq!(
            events,
            vec![Event::Accepted {
                account_id: AccountId(2),
                order_id: OrderId(2),
                resting_qty: Qty(3),
            }],
            "a deeper resting order must not produce a BookUpdate -- the touch didn't move"
        );
        engine.book().assert_invariants();
    }

    #[test]
    fn book_update_reflects_qty_change_at_the_touch_from_a_partial_fill() {
        let mut engine = Engine::new();
        engine.apply(
            Command::NewOrder {
                account_id: AccountId(1),
                order_id: OrderId(1),
                side: Side::Sell,
                price: Price(100),
                qty: Qty(10),
                kind: OrderKind::Limit,
                tif: Tif::Gtc,
                client_ts: 0,
            },
            &mut |_| {},
        );

        let mut events = Vec::new();
        engine.apply(
            Command::NewOrder {
                account_id: AccountId(2),
                order_id: OrderId(2),
                side: Side::Buy,
                price: Price(100),
                qty: Qty(4),
                kind: OrderKind::Limit,
                tif: Tif::Gtc,
                client_ts: 0,
            },
            &mut |e| events.push(e),
        );
        // Price at the touch is unchanged (still 100), but the resting
        // qty there dropped from 10 to 6 -- that is still a book change
        // to the touch and must still produce a BookUpdate.
        assert_eq!(
            events.last(),
            Some(&Event::BookUpdate {
                best_bid: None,
                best_ask: Some((Price(100), Qty(6))),
            })
        );
        engine.book().assert_invariants();
    }

    #[test]
    fn apply_kill_switch_is_inert_in_engine() {
        // Kill-switch state lives in risk, checked before apply is ever
        // called -- Engine itself has nothing to do with it.
        let mut engine = Engine::new();
        let mut events = Vec::new();
        engine.apply(Command::KillSwitch { engaged: true }, &mut |e| {
            events.push(e)
        });
        assert!(events.is_empty());
        engine.book().assert_invariants();
    }

    #[test]
    fn apply_snapshot_on_an_empty_book_emits_only_the_summary_trailer() {
        let mut engine = Engine::new();
        let mut events = Vec::new();
        engine.apply(Command::Snapshot, &mut |e| events.push(e));
        assert_eq!(
            events,
            vec![Event::SnapshotSummary {
                best_bid: None,
                best_ask: None,
                last_trade: None,
                level_count: 0,
                account_count: 0,
            }]
        );
        engine.book().assert_invariants();
    }

    #[test]
    fn apply_snapshot_reflects_resting_state_and_never_emits_book_update() {
        let mut engine = Engine::new();
        engine.apply(
            Command::NewOrder {
                account_id: AccountId(1),
                order_id: OrderId(1),
                side: Side::Buy,
                price: Price(100),
                qty: Qty(5),
                kind: OrderKind::Limit,
                tif: Tif::Gtc,
                client_ts: 0,
            },
            &mut |_| {},
        );

        let mut events = Vec::new();
        engine.apply(Command::Snapshot, &mut |e| events.push(e));
        assert_eq!(
            events,
            vec![
                Event::SnapshotLevel {
                    side: Side::Buy,
                    price: Price(100),
                    qty: Qty(5),
                    order_count: 1,
                },
                Event::SnapshotAccount {
                    account_id: AccountId(1),
                    open_order_count: 1,
                    notional: 500,
                },
                Event::SnapshotSummary {
                    best_bid: Some((Price(100), Qty(5))),
                    best_ask: None,
                    last_trade: None,
                    level_count: 1,
                    account_count: 1,
                },
            ],
            "Snapshot is read-only -- it must never itself trigger a BookUpdate"
        );
        engine.book().assert_invariants();
    }
}
