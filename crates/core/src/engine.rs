//! The single entry point the matching thread calls (SPEC §4:
//! `Engine::apply`). `Engine` knows nothing about sockets, connections, or
//! sequence numbers — it only ever sees `Command` in, `Event` out.

use crate::book::Book;
use crate::event::{Command, Event};
use crate::types::{OrderKind, Tif};

/// Wraps `Book` behind the one call site SPEC §4 names: `Engine::apply`.
///
/// `KillSwitch` and `Snapshot` are accepted here (so `apply` stays total
/// over every `Command` variant, since all six travel the same channel
/// from the gateway) but not yet acted on — kill-switch *state* belongs to
/// `risk` (SPEC §4), checked by the matching thread *before* calling
/// `apply`, not inside it; real handling lands with stage 4/marketdata's
/// snapshot support. Applying either variant here is currently a no-op
/// that emits nothing, not a guess at behaviour that isn't built yet.
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
    pub fn apply(&mut self, cmd: Command, emit: &mut dyn FnMut(Event)) {
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
            Command::KillSwitch { .. } | Command::Snapshot => {}
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
            vec![Event::Accepted {
                account_id: AccountId(1),
                order_id: OrderId(1),
                resting_qty: Qty(5),
            }]
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
            vec![Event::Cancelled {
                account_id: AccountId(1),
                order_id: OrderId(1),
            }]
        );
        engine.book().assert_invariants();
        assert_eq!(engine.book().best_bid(), None);
    }

    #[test]
    fn apply_kill_switch_and_snapshot_are_inert_for_now() {
        let mut engine = Engine::new();
        let mut events = Vec::new();
        engine.apply(Command::KillSwitch { engaged: true }, &mut |e| {
            events.push(e)
        });
        engine.apply(Command::Snapshot, &mut |e| events.push(e));
        assert!(events.is_empty());
        engine.book().assert_invariants();
    }
}
