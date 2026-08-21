//! Kill switch, per-account limits, price band (SPEC §5).
//!
//! Depends on `core` only. Checked on the matching thread before
//! `Engine::apply`, on every command.
//!
//! Account open-order counts and notional are already maintained by
//! `Book::rest`/`Book::unlink` since stage 1 — this crate only reads them
//! (`Book::account_open_order_count`, `Book::account_notional`, both
//! O(1)). It adds one new piece of bookkeeping stage 1 had no reason to
//! need: `Book::last_trade`, since nothing before this stage cared what
//! the book last traded at. Everything else here is checks, not
//! maintenance.

mod config;

pub use config::RiskConfig;

use core::Book;
use core::Engine;
use core::error::RejectReason;
use core::event::{Command, Event};
use core::types::{AccountId, EngineSeq, OrderId, OrderKind, Price, Side, Tif};

/// Runs one command through the exact risk-then-matching path live traffic
/// uses: risk first, and only if risk permits, `Engine::apply` (SPEC §5).
/// Replay (stage 6, SPEC §7) calls this same function against a fresh
/// `Engine`/`RiskState` rather than a parallel reimplementation, so a
/// command rejected live is guaranteed to be rejected identically on
/// replay -- not just "expected to," by construction.
///
/// `engine_seq` is incremented exactly once here, for every command,
/// whether it is risk-rejected below or reaches `Engine::apply` --
/// `EngineSeq` counts every command the *system* processes (SPEC §2), not
/// only ones the engine actually applied. This is the one and only
/// increment site: the live matching thread and `bin::replay_file` both
/// call this same function, so they cannot drift from each other by
/// convention -- there is no second call site left to forget.
pub fn process_command(
    engine: &mut Engine,
    risk_state: &mut RiskState,
    cmd: Command,
    engine_seq: &mut EngineSeq,
    emit: &mut dyn FnMut(EngineSeq, Event),
) {
    engine_seq.0 += 1;
    let seq = *engine_seq;
    if let Some(rejected) = risk_state.pre_apply_check(&cmd, engine.book()) {
        emit(seq, rejected);
        return;
    }
    engine.apply(cmd, &mut |event| emit(seq, event));
}

/// Kill switch + per-account limits + price band, owned by the matching
/// thread and checked before every `Engine::apply` call (SPEC §5).
#[derive(Debug)]
pub struct RiskState {
    config: RiskConfig,
    kill_switch_engaged: bool,
}

impl RiskState {
    pub fn new(config: RiskConfig) -> Self {
        Self {
            config,
            kill_switch_engaged: false,
        }
    }

    /// Pre-`apply` gate. `Some(reject)` means `cmd` must never reach
    /// `Engine::apply`; `None` means proceed.
    ///
    /// A `KillSwitch` command updates `self`'s own state as a side effect
    /// and always returns `None` — toggling the switch is never itself
    /// rejected, and is observed **inside the matching loop** (this is
    /// that observation point), not only at ingress, so a command already
    /// sitting in the channel when the switch fires is still checked
    /// against the new state (SPEC §5).
    pub fn pre_apply_check(&mut self, cmd: &Command, book: &Book) -> Option<Event> {
        if let Command::KillSwitch { engaged } = cmd {
            self.kill_switch_engaged = *engaged;
            return None;
        }

        if self.kill_switch_engaged
            && let Some(reject) = self.kill_switch_check(cmd, book)
        {
            return Some(reject);
        }

        if let Command::NewOrder {
            account_id,
            order_id,
            side,
            price,
            qty,
            kind,
            tif,
            client_ts: _,
        } = *cmd
        {
            return self.new_order_check(account_id, order_id, side, price, qty, kind, tif, book);
        }

        None
    }

    /// Drain policy (SPEC §5): new order entry is rejected. `Cancel` and
    /// `MassCancel` are pure removals and always allowed. `CancelReplace`
    /// is allowed only if it strictly decreases exposure (quantity down
    /// or unchanged, price unchanged) — an increase or a reprice is "a
    /// new economic commitment" by §2's own modify rationale, and drain
    /// halts new commitments. An amend targeting an order that doesn't
    /// exist (wrong account, unknown id) is let through here — there is
    /// no commitment to block, and `Book::modify`'s own no-oracle check
    /// will reject it identically to how it always does.
    fn kill_switch_check(&self, cmd: &Command, book: &Book) -> Option<Event> {
        match *cmd {
            Command::NewOrder {
                account_id,
                order_id,
                ..
            } => Some(Event::Rejected {
                account_id,
                order_id,
                reason: RejectReason::KillSwitchActive,
            }),
            Command::CancelOrder { .. } | Command::MassCancel { .. } => None,
            Command::CancelReplace {
                account_id,
                order_id,
                new_price,
                new_qty,
            } => {
                let (old_price, old_qty) = book.resting_order_snapshot(account_id, order_id)?;
                let is_new_commitment = new_price != old_price || new_qty.0 > old_qty.0;
                if is_new_commitment {
                    Some(Event::Rejected {
                        account_id,
                        order_id,
                        reason: RejectReason::KillSwitchActive,
                    })
                } else {
                    None
                }
            }
            Command::KillSwitch { .. } | Command::Snapshot => None,
        }
    }

    /// Per-account limits and price band for a `NewOrder` (SPEC §5).
    #[allow(clippy::too_many_arguments)]
    fn new_order_check(
        &self,
        account_id: AccountId,
        order_id: OrderId,
        side: Side,
        price: Price,
        qty: core::types::Qty,
        kind: OrderKind,
        tif: Tif,
        book: &Book,
    ) -> Option<Event> {
        let reject = |reason: RejectReason| {
            Some(Event::Rejected {
                account_id,
                order_id,
                reason,
            })
        };

        // max_open_orders: only order types that can actually rest can
        // violate it -- IOC/FOK/Market never add a resting order
        // regardless of the count, so checking them against this cap
        // would reject on a condition they cannot trigger.
        let can_rest = kind == OrderKind::Limit && matches!(tif, Tif::Gtc | Tif::PostOnly);
        if can_rest && book.account_open_order_count(account_id) >= self.config.max_open_orders {
            return reject(RejectReason::MaxOpenOrders);
        }

        // Notional. Market's wire price is unused/zero (SPEC §2) -- using
        // it here would read every Market order as zero notional, an
        // unbounded order being the dangerous case, not an exempt one.
        // Resolution is last trade, else the best price on the side being
        // swept (not the band's two-sided mid -- a Market order only
        // needs depth on the side it sweeps), else NotFullyFillable, the
        // same "nothing to fill" outcome §2 already describes for that
        // case, not a new rejection.
        let effective_price = match kind {
            OrderKind::Limit => price,
            OrderKind::Market => match self.market_reference_price(side, book) {
                Some(p) => p,
                None => return reject(RejectReason::NotFullyFillable),
            },
        };
        let incoming_notional = effective_price.0 as u128 * qty.0 as u128;
        let total_notional = book.account_notional(account_id) + incoming_notional;
        if total_notional > self.config.max_notional {
            return reject(RejectReason::MaxNotional);
        }

        // Price band. Does not apply to Market orders (SPEC §5): a Market
        // order can only fill against already-rested, already-banded
        // depth.
        if kind == OrderKind::Limit
            && let Some(reference) = self.price_band_reference(book)
        {
            let band = u128::from(self.config.price_band_pct);
            let reference = u128::from(reference.0);
            let lower = reference * (100 - band) / 100;
            let upper = reference * (100 + band) / 100;
            let p = u128::from(price.0);
            if p < lower || p > upper {
                return reject(RejectReason::PriceBandViolation);
            }
        }

        None
    }

    /// A Market order's notional reference: last trade, else the best
    /// price on the side it would sweep. `None` means no depth exists on
    /// that side and no trade has ever occurred -- nothing to fill
    /// against (SPEC §5).
    fn market_reference_price(&self, side: Side, book: &Book) -> Option<Price> {
        if let Some(last) = book.last_trade() {
            return Some(last);
        }
        match side {
            Side::Buy => book.best_ask(),
            Side::Sell => book.best_bid(),
        }
    }

    /// The price band's reference: last trade, else the book mid if both
    /// sides have depth, else `None` (check skipped) (SPEC §5).
    fn price_band_reference(&self, book: &Book) -> Option<Price> {
        if let Some(last) = book.last_trade() {
            return Some(last);
        }
        match (book.best_bid(), book.best_ask()) {
            (Some(bid), Some(ask)) => Some(Price((bid.0 + ask.0) / 2)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::event::Event;
    use core::types::Qty;

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

    fn market_order(account: u64, order: u64, side: Side, qty: u64) -> Command {
        Command::NewOrder {
            account_id: AccountId(account),
            order_id: OrderId(order),
            side,
            price: Price(0),
            qty: Qty(qty),
            kind: OrderKind::Market,
            tif: Tif::Ioc,
            client_ts: 0,
        }
    }

    fn rejected(account: u64, order: u64, reason: RejectReason) -> Option<Event> {
        Some(Event::Rejected {
            account_id: AccountId(account),
            order_id: OrderId(order),
            reason,
        })
    }

    // -- Kill switch -------------------------------------------------

    #[test]
    fn kill_switch_rejects_new_order_entry() {
        let mut risk = RiskState::new(RiskConfig::default());
        let book = Book::new();
        risk.pre_apply_check(&Command::KillSwitch { engaged: true }, &book);

        let cmd = new_order(1, 1, Side::Buy, 100, 5);
        assert_eq!(
            risk.pre_apply_check(&cmd, &book),
            rejected(1, 1, RejectReason::KillSwitchActive)
        );
    }

    #[test]
    fn kill_switch_rejects_a_command_processed_after_it_fires_even_if_queued_earlier() {
        let mut risk = RiskState::new(RiskConfig::default());
        let book = Book::new();

        // Order A is conceptually already sitting in the channel,
        // decoded and validated, waiting its turn -- represented here by
        // simply not having been checked yet. The kill switch reaches
        // the matching loop and is processed first.
        let switch_result = risk.pre_apply_check(&Command::KillSwitch { engaged: true }, &book);
        assert_eq!(
            switch_result, None,
            "toggling the switch itself is never rejected"
        );

        // Only now is order A actually checked. It must still be
        // rejected: the check reads current state at processing time,
        // there is no snapshot taken back when it was queued.
        let order_a = new_order(1, 1, Side::Buy, 100, 5);
        assert_eq!(
            risk.pre_apply_check(&order_a, &book),
            rejected(1, 1, RejectReason::KillSwitchActive)
        );
    }

    #[test]
    fn kill_switch_allows_cancel_and_mass_cancel_during_drain() {
        let mut risk = RiskState::new(RiskConfig::default());
        let book = Book::new();
        risk.pre_apply_check(&Command::KillSwitch { engaged: true }, &book);

        let cancel = Command::CancelOrder {
            account_id: AccountId(1),
            order_id: OrderId(1),
        };
        assert_eq!(risk.pre_apply_check(&cancel, &book), None);

        let mass_cancel = Command::MassCancel {
            account_id: AccountId(1),
        };
        assert_eq!(risk.pre_apply_check(&mass_cancel, &book), None);
    }

    #[test]
    fn kill_switch_allows_cancel_replace_that_only_decreases_quantity() {
        let mut book = Book::new();
        book.submit_gtc(
            AccountId(1),
            OrderId(1),
            Side::Sell,
            Price(100),
            Qty(5),
            &mut |_| {},
        );
        let mut risk = RiskState::new(RiskConfig::default());
        risk.pre_apply_check(&Command::KillSwitch { engaged: true }, &book);

        let decrease = Command::CancelReplace {
            account_id: AccountId(1),
            order_id: OrderId(1),
            new_price: Price(100),
            new_qty: Qty(2),
        };
        assert_eq!(risk.pre_apply_check(&decrease, &book), None);

        let unchanged = Command::CancelReplace {
            account_id: AccountId(1),
            order_id: OrderId(1),
            new_price: Price(100),
            new_qty: Qty(5),
        };
        assert_eq!(risk.pre_apply_check(&unchanged, &book), None);
    }

    #[test]
    fn kill_switch_blocks_cancel_replace_that_increases_quantity_or_reprices() {
        let mut book = Book::new();
        book.submit_gtc(
            AccountId(1),
            OrderId(1),
            Side::Sell,
            Price(100),
            Qty(5),
            &mut |_| {},
        );
        let mut risk = RiskState::new(RiskConfig::default());
        risk.pre_apply_check(&Command::KillSwitch { engaged: true }, &book);

        let increase = Command::CancelReplace {
            account_id: AccountId(1),
            order_id: OrderId(1),
            new_price: Price(100),
            new_qty: Qty(6),
        };
        assert_eq!(
            risk.pre_apply_check(&increase, &book),
            rejected(1, 1, RejectReason::KillSwitchActive)
        );

        let reprice = Command::CancelReplace {
            account_id: AccountId(1),
            order_id: OrderId(1),
            new_price: Price(101),
            new_qty: Qty(5),
        };
        assert_eq!(
            risk.pre_apply_check(&reprice, &book),
            rejected(1, 1, RejectReason::KillSwitchActive)
        );
    }

    #[test]
    fn kill_switch_cancel_replace_for_an_order_that_does_not_exist_is_not_blocked_here() {
        // No resting order at all -- risk lets it through rather than
        // guessing; Book::modify's own no-oracle rejection (UnknownOrderId)
        // handles it downstream, identically for a genuinely-unknown id
        // and a wrong-account one.
        let book = Book::new();
        let mut risk = RiskState::new(RiskConfig::default());
        risk.pre_apply_check(&Command::KillSwitch { engaged: true }, &book);

        let modify = Command::CancelReplace {
            account_id: AccountId(1),
            order_id: OrderId(1),
            new_price: Price(200),
            new_qty: Qty(10),
        };
        assert_eq!(risk.pre_apply_check(&modify, &book), None);
    }

    #[test]
    fn kill_switch_disengaging_allows_new_order_entry_again() {
        let mut risk = RiskState::new(RiskConfig::default());
        let book = Book::new();
        risk.pre_apply_check(&Command::KillSwitch { engaged: true }, &book);
        risk.pre_apply_check(&Command::KillSwitch { engaged: false }, &book);

        let cmd = new_order(1, 1, Side::Buy, 100, 5);
        assert_eq!(risk.pre_apply_check(&cmd, &book), None);
    }

    // -- max_open_orders -----------------------------------------------

    #[test]
    fn max_open_orders_breach_at_exactly_the_cap_boundary() {
        let mut book = Book::new();
        let mut risk = RiskState::new(RiskConfig::default());

        for i in 0..49u64 {
            book.submit_gtc(
                AccountId(1),
                OrderId(i),
                Side::Sell,
                Price(1000 + i),
                Qty(1),
                &mut |_| {},
            );
        }
        assert_eq!(book.account_open_order_count(AccountId(1)), 49);

        // The 50th order fills the cap exactly -- allowed.
        let order_50 = new_order(1, 49, Side::Sell, 1049, 1);
        assert_eq!(risk.pre_apply_check(&order_50, &book), None);
        book.submit_gtc(
            AccountId(1),
            OrderId(49),
            Side::Sell,
            Price(1049),
            Qty(1),
            &mut |_| {},
        );
        assert_eq!(book.account_open_order_count(AccountId(1)), 50);

        // The 51st breaches it.
        let order_51 = new_order(1, 50, Side::Sell, 1050, 1);
        assert_eq!(
            risk.pre_apply_check(&order_51, &book),
            rejected(1, 50, RejectReason::MaxOpenOrders)
        );
    }

    #[test]
    fn max_open_orders_does_not_apply_to_order_types_that_never_rest() {
        let mut book = Book::new();
        let mut risk = RiskState::new(RiskConfig::default());
        for i in 0..50u64 {
            book.submit_gtc(
                AccountId(1),
                OrderId(i),
                Side::Sell,
                Price(1000 + i),
                Qty(1),
                &mut |_| {},
            );
        }
        assert_eq!(book.account_open_order_count(AccountId(1)), 50);

        // IOC never rests, regardless of the account's open-order count --
        // it structurally cannot violate a cap on *resting* orders.
        let ioc = Command::NewOrder {
            account_id: AccountId(1),
            order_id: OrderId(999),
            side: Side::Buy,
            price: Price(1000),
            qty: Qty(1),
            kind: OrderKind::Limit,
            tif: Tif::Ioc,
            client_ts: 0,
        };
        assert_eq!(risk.pre_apply_check(&ioc, &book), None);
    }

    // -- Notional --------------------------------------------------------

    #[test]
    fn max_notional_breach() {
        let book = Book::new();
        let mut risk = RiskState::new(RiskConfig::default());
        // 100_000 * 1_001 = 100_100_000 > 100_000_000 cap.
        let cmd = new_order(1, 1, Side::Buy, 100_000, 1_001);
        assert_eq!(
            risk.pre_apply_check(&cmd, &book),
            rejected(1, 1, RejectReason::MaxNotional)
        );
    }

    #[test]
    fn notional_is_gross_both_sides_summed() {
        let mut book = Book::new();
        let mut risk = RiskState::new(RiskConfig::default());
        // A two-sided quote of 50_000_000 on each side consumes the full
        // 100_000_000 cap (SPEC §5: "$1,000,000") -- gross, not net (bid
        // minus ask would read as ~0 here).
        book.submit_gtc(
            AccountId(1),
            OrderId(1),
            Side::Buy,
            Price(100_000),
            Qty(500),
            &mut |_| {},
        ); // 50_000_000
        book.submit_gtc(
            AccountId(1),
            OrderId(2),
            Side::Sell,
            Price(200_000),
            Qty(250),
            &mut |_| {},
        ); // 50_000_000
        assert_eq!(book.account_notional(AccountId(1)), 100_000_000);

        // Any further notional at all now breaches the cap. Priced at the
        // mid (150_000, within the ±10% band) so this is unambiguously a
        // notional rejection, not a price-band one.
        let cmd = new_order(1, 3, Side::Buy, 150_000, 1);
        assert_eq!(
            risk.pre_apply_check(&cmd, &book),
            rejected(1, 3, RejectReason::MaxNotional)
        );
    }

    #[test]
    fn notional_arithmetic_near_u64_max_does_not_wrap() {
        let book = Book::new();
        let mut risk = RiskState::new(RiskConfig::default());
        // price * qty here exceeds u64::MAX (~1.8e19): 5e9 * 5e9 = 2.5e19.
        // u128 arithmetic must not panic or silently wrap; it must simply
        // compare correctly against the (much smaller) cap and reject.
        let cmd = new_order(1, 1, Side::Buy, 5_000_000_000, 5_000_000_000);
        assert_eq!(
            risk.pre_apply_check(&cmd, &book),
            rejected(1, 1, RejectReason::MaxNotional)
        );
    }

    // -- Market order notional -------------------------------------------

    #[test]
    fn market_order_notional_uses_reference_price_and_breaches_cap_against_thin_liquidity() {
        let mut book = Book::new();
        let mut risk = RiskState::new(RiskConfig::default());
        // Thin foreign liquidity at a high price -- if the wire's unused
        // price = 0 were read literally, this would show zero notional
        // and never breach anything, which is the opposite of a cap.
        book.submit_gtc(
            AccountId(2),
            OrderId(1),
            Side::Sell,
            Price(1_000_000),
            Qty(1_000),
            &mut |_| {},
        );
        let cmd = market_order(1, 1, Side::Buy, 1_000);
        assert_eq!(
            risk.pre_apply_check(&cmd, &book),
            rejected(1, 1, RejectReason::MaxNotional)
        );
    }

    #[test]
    fn market_order_with_depth_only_on_swept_side_fills_normally_not_rejected() {
        let mut book = Book::new();
        let mut risk = RiskState::new(RiskConfig::default());
        // Only asks exist -- no bids, so the price band's two-sided mid
        // would be undefined. A Market buy only needs depth on the ask
        // side it actually sweeps, and must not be rejected for lack of
        // a mid that a Market order was never going to use anyway.
        book.submit_gtc(
            AccountId(2),
            OrderId(1),
            Side::Sell,
            Price(100),
            Qty(1_000),
            &mut |_| {},
        );
        let cmd = market_order(1, 1, Side::Buy, 5);
        assert_eq!(risk.pre_apply_check(&cmd, &book), None);
    }

    #[test]
    fn market_order_rejected_not_fully_fillable_when_no_depth_and_no_trade() {
        let book = Book::new();
        let mut risk = RiskState::new(RiskConfig::default());
        let cmd = market_order(1, 1, Side::Buy, 5);
        assert_eq!(
            risk.pre_apply_check(&cmd, &book),
            rejected(1, 1, RejectReason::NotFullyFillable)
        );
    }

    // -- Price band --------------------------------------------------------

    #[test]
    fn price_band_rejects_above_and_below() {
        let mut book = Book::new();
        let mut risk = RiskState::new(RiskConfig::default());
        // Two-sided depth at distinct, non-crossing prices -> mid
        // reference is (99 + 101) / 2 = 100, band is [90, 110].
        book.submit_gtc(
            AccountId(2),
            OrderId(1),
            Side::Buy,
            Price(99),
            Qty(1),
            &mut |_| {},
        );
        book.submit_gtc(
            AccountId(2),
            OrderId(2),
            Side::Sell,
            Price(101),
            Qty(1),
            &mut |_| {},
        );

        let above = new_order(1, 10, Side::Buy, 111, 1);
        assert_eq!(
            risk.pre_apply_check(&above, &book),
            rejected(1, 10, RejectReason::PriceBandViolation)
        );

        let below = new_order(1, 11, Side::Sell, 89, 1);
        assert_eq!(
            risk.pre_apply_check(&below, &book),
            rejected(1, 11, RejectReason::PriceBandViolation)
        );

        // Just inside the band on both sides is allowed.
        let just_above = new_order(1, 12, Side::Sell, 110, 1);
        assert_eq!(risk.pre_apply_check(&just_above, &book), None);
        let just_below = new_order(1, 13, Side::Buy, 90, 1);
        assert_eq!(risk.pre_apply_check(&just_below, &book), None);
    }

    #[test]
    fn price_band_skipped_on_an_empty_book() {
        let book = Book::new();
        let mut risk = RiskState::new(RiskConfig::default());
        // Wildly-priced order, but there is no reference at all yet.
        let cmd = new_order(1, 1, Side::Buy, 1_000_000, 1);
        assert_eq!(risk.pre_apply_check(&cmd, &book), None);
    }

    #[test]
    fn price_band_skipped_on_a_one_sided_book() {
        let mut book = Book::new();
        let mut risk = RiskState::new(RiskConfig::default());
        // Bids only, no asks -- a mid is undefined with only one side
        // (SPEC §5), so the check must be skipped, not derived from a
        // single side.
        book.submit_gtc(
            AccountId(2),
            OrderId(1),
            Side::Buy,
            Price(100),
            Qty(1),
            &mut |_| {},
        );
        let cmd = new_order(1, 1, Side::Buy, 1_000_000, 1);
        assert_eq!(risk.pre_apply_check(&cmd, &book), None);
    }

    #[test]
    fn price_band_prefers_last_trade_over_mid_once_a_trade_has_occurred() {
        let mut book = Book::new();
        let mut risk = RiskState::new(RiskConfig::default());
        // A trade at 100, then fresh two-sided depth far from 100 -- the
        // reference must stay the last trade price, not the new mid.
        book.submit_gtc(
            AccountId(2),
            OrderId(1),
            Side::Sell,
            Price(100),
            Qty(5),
            &mut |_| {},
        );
        book.submit_gtc(
            AccountId(3),
            OrderId(2),
            Side::Buy,
            Price(100),
            Qty(5),
            &mut |_| {},
        );
        assert_eq!(book.last_trade(), Some(Price(100)));

        book.submit_gtc(
            AccountId(2),
            OrderId(3),
            Side::Buy,
            Price(500),
            Qty(1),
            &mut |_| {},
        );
        book.submit_gtc(
            AccountId(2),
            OrderId(4),
            Side::Sell,
            Price(700),
            Qty(1),
            &mut |_| {},
        );
        // Mid of the new depth would be 600, putting 111 outside a
        // 600-based band but also outside a 100-based one -- pick a
        // price that's inside the 100-based band and clearly outside a
        // 600-based one, to prove which reference actually won.
        let cmd = new_order(1, 10, Side::Buy, 109, 1);
        assert_eq!(
            risk.pre_apply_check(&cmd, &book),
            None,
            "109 is within [90, 110] of last trade 100 -- must not be judged against the new mid"
        );
    }

    #[test]
    fn price_band_does_not_apply_to_market_orders() {
        let mut book = Book::new();
        let mut risk = RiskState::new(RiskConfig::default());
        book.submit_gtc(
            AccountId(2),
            OrderId(1),
            Side::Buy,
            Price(99),
            Qty(1),
            &mut |_| {},
        );
        book.submit_gtc(
            AccountId(2),
            OrderId(2),
            Side::Sell,
            Price(101),
            Qty(1),
            &mut |_| {},
        );
        // A Market buy sweeps the ask at 101 -- nowhere near the band's
        // reference range if it were (wrongly) checked against price 0.
        let cmd = market_order(1, 1, Side::Buy, 1);
        assert_eq!(risk.pre_apply_check(&cmd, &book), None);
    }
}
