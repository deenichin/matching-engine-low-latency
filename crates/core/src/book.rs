//! The order book itself: the two price maps, the arena, and the indexes
//! that make cancel, modify, and per-account risk checks O(1).
//!
//! `rest` and `unlink` are the only two places that touch an
//! `AccountEntry::slots` array or a `Node::acct_idx` — every mutating
//! operation (`submit_gtc`, `cancel`, `mass_cancel`) is built on top of
//! them rather than duplicating that maintenance.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::account::AccountEntry;
use crate::arena::{Arena, NULL, Node};
use crate::error::RejectReason;
use crate::event::Event;
use crate::level::Level;
use crate::types::{AccountId, FillState, OrderId, Price, Qty, Side};

/// One side of a top-of-book snapshot: the best price and the total
/// resting quantity there, or `None` if that side is empty.
pub type TouchSide = Option<(Price, Qty)>;

/// `Filled.state` is a pure function of the fill's own `resting_qty`
/// (SPEC §3) — computed once, here, rather than inline at each of
/// `sweep`'s two `Filled` construction sites, so the two can't drift.
fn fill_state(resting_qty: Qty) -> FillState {
    if resting_qty.0 == 0 {
        FillState::Filled
    } else {
        FillState::PartiallyFilled
    }
}

/// The single-symbol order book: two price maps, the resting-order arena,
/// and the indexes that make cancel, modify, and per-account risk checks
/// O(1) (SPEC §4).
#[derive(Debug)]
pub struct Book {
    pub(crate) bids: BTreeMap<Price, Level>,
    pub(crate) asks: BTreeMap<Price, Level>,
    pub(crate) arena: Arena,
    /// Keyed on `(AccountId, OrderId)`, never bare `OrderId` — `OrderId` is
    /// only unique per account (SPEC §2), so two different accounts may
    /// legally submit the same numeric id concurrently. A bare
    /// `HashMap<OrderId, _>` here is exactly the bug this key exists to
    /// prevent (SPEC §4).
    pub(crate) order_index: HashMap<(AccountId, OrderId), u32>,
    pub(crate) accounts: HashMap<AccountId, AccountEntry>,
    /// Price of the most recent fill, if any. Risk's price band and
    /// Market-order notional resolution both prefer this over a computed
    /// mid once the book has traded (SPEC §5). New bookkeeping for stage
    /// 4 -- nothing needed this in stage 1.
    pub(crate) last_trade: Option<Price>,
    /// Reused scratch space for `mass_cancel`'s slot snapshot -- avoids
    /// `entry.slots.clone()` allocating fresh on every call (SPEC §6, §9:
    /// found by the `hot_path_allocates_nothing` allocation test). Starts
    /// empty and grows to whatever the largest single `mass_cancel` call
    /// has needed so far, then never shrinks -- the same "reserve once,
    /// reuse forever" shape as `AccountEntry::slots`, just discovered for
    /// this buffer later.
    mass_cancel_scratch: Vec<u32>,
}

impl Book {
    pub fn new() -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            arena: Arena::new(),
            order_index: HashMap::new(),
            accounts: HashMap::new(),
            last_trade: None,
            mass_cancel_scratch: Vec::new(),
        }
    }

    /// Best bid: the highest resting buy price, or `None` if the bid side
    /// is empty. `BTreeMap`'s last key (SPEC §4).
    pub fn best_bid(&self) -> Option<Price> {
        self.bids.keys().next_back().copied()
    }

    /// Best ask: the lowest resting sell price, or `None` if the ask side
    /// is empty. `BTreeMap`'s first key (SPEC §4).
    pub fn best_ask(&self) -> Option<Price> {
        self.asks.keys().next().copied()
    }

    /// Price of the most recent fill, or `None` if the book has never
    /// traded.
    pub fn last_trade(&self) -> Option<Price> {
        self.last_trade
    }

    /// Top-of-book snapshot: best bid/ask price and the total resting
    /// quantity at that price, or `None` on a side that's empty. What a
    /// `BookUpdate` event carries (SPEC §8).
    pub fn top_of_book(&self) -> (TouchSide, TouchSide) {
        let bid = self
            .bids
            .iter()
            .next_back()
            .map(|(&price, level)| (price, level.total_qty));
        let ask = self
            .asks
            .iter()
            .next()
            .map(|(&price, level)| (price, level.total_qty));
        (bid, ask)
    }

    /// An account's current open-order count, O(1) (SPEC §4/§5). Zero for
    /// an account that has never rested an order.
    pub fn account_open_order_count(&self, account_id: AccountId) -> usize {
        self.accounts
            .get(&account_id)
            .map_or(0, |entry| entry.open_order_count())
    }

    /// An account's current gross notional, O(1) (SPEC §5). Zero for an
    /// account that has never rested an order.
    pub fn account_notional(&self, account_id: AccountId) -> u128 {
        self.accounts
            .get(&account_id)
            .map_or(0, |entry| entry.notional())
    }

    /// A specific resting order's current price/qty, O(1) via the order
    /// index -- for risk checks that need to compare a proposed amend
    /// against what's actually resting (e.g. is this `CancelReplace` an
    /// increase or a decrease). `None` if the account has no such
    /// resting order (unknown id, wrong account, or already gone).
    pub fn resting_order_snapshot(
        &self,
        account_id: AccountId,
        order_id: OrderId,
    ) -> Option<(Price, Qty)> {
        let &slot = self.order_index.get(&(account_id, order_id))?;
        let node = self.arena.get(slot)?;
        Some((node.price, node.qty))
    }

    /// Insert a brand-new resting node at the back of its price level's
    /// queue, creating the level if this is the first order at that price
    /// and the account's index entry if this is the first time the account
    /// has been seen.
    ///
    /// One of the **only two** places (with [`Book::unlink`]) that touch
    /// `AccountEntry::slots` or `Node::acct_idx`.
    fn rest(
        &mut self,
        account_id: AccountId,
        order_id: OrderId,
        side: Side,
        price: Price,
        qty: Qty,
    ) {
        let acct_idx = self.accounts.entry(account_id).or_default().slots.len() as u32;

        let slot = self.arena.insert(Node {
            id: order_id,
            account: account_id,
            price,
            qty,
            side,
            prev: NULL,
            next: NULL,
            acct_idx,
        });

        let entry = self
            .accounts
            .get_mut(&account_id)
            .expect("just touched via entry().or_default() above");
        entry.slots.push(slot);
        entry.notional += price.0 as u128 * qty.0 as u128;

        self.order_index.insert((account_id, order_id), slot);

        let map = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        let level = map.entry(price).or_default();
        let old_tail = level.tail;
        level.tail = slot;
        if old_tail == NULL {
            level.head = slot;
        }
        level.count += 1;
        level.total_qty.0 += qty.0;

        if old_tail != NULL {
            self.arena
                .get_mut(old_tail)
                .expect("old tail must be live")
                .next = slot;
            self.arena.get_mut(slot).expect("just inserted").prev = old_tail;
        }
    }

    /// Remove a resting order from the book entirely: unlink it from its
    /// price level's chain (removing the level from the map if that was
    /// its last order), remove it from the order index, remove it from its
    /// account's slot array (fixing up whichever element the `swap_remove`
    /// moves into the vacated position), update the account's notional,
    /// and free its arena slot.
    ///
    /// One of the **only two** places (with [`Book::rest`]) that touch
    /// `AccountEntry::slots` or `Node::acct_idx`. Any other path that
    /// removes a node from a level without going through here desyncs the
    /// account index silently.
    fn unlink(&mut self, slot: u32) {
        let node = *self.arena.get(slot).expect("unlink called on a live slot");

        if node.prev != NULL {
            self.arena
                .get_mut(node.prev)
                .expect("prev must be live")
                .next = node.next;
        }
        if node.next != NULL {
            self.arena
                .get_mut(node.next)
                .expect("next must be live")
                .prev = node.prev;
        }
        {
            let map = match node.side {
                Side::Buy => &mut self.bids,
                Side::Sell => &mut self.asks,
            };
            let level = map
                .get_mut(&node.price)
                .expect("level must exist for a resting node's price");
            if level.head == slot {
                level.head = node.next;
            }
            if level.tail == slot {
                level.tail = node.prev;
            }
            level.count -= 1;
            level.total_qty.0 -= node.qty.0;
            if level.is_empty() {
                map.remove(&node.price);
            }
        }

        self.order_index.remove(&(node.account, node.id));

        {
            let entry = self
                .accounts
                .get_mut(&node.account)
                .expect("node's account must have an entry");
            let removed = entry.slots.swap_remove(node.acct_idx as usize);
            debug_assert_eq!(
                removed, slot,
                "acct_idx did not point back to the slot being unlinked"
            );
            entry.notional -= node.price.0 as u128 * node.qty.0 as u128;
            if let Some(&moved_slot) = entry.slots.get(node.acct_idx as usize) {
                self.arena
                    .get_mut(moved_slot)
                    .expect("moved slot must be live")
                    .acct_idx = node.acct_idx;
            }
        }

        self.arena.free_slot(slot);
    }

    /// Reduce a maker's resting quantity after a partial fill. Does *not*
    /// touch the order index or `AccountEntry::slots`/`acct_idx` — the node
    /// stays in the same slot, same level, same position in its account's
    /// slots array, so none of that indexing moves. Only the level's cached
    /// `total_qty` and the account's `notional` (aggregates, not indexes)
    /// need to shrink with it.
    fn reduce_resting_qty(&mut self, slot: u32, side: Side, price: Price, fill_qty: u64) {
        let account = {
            let node = self
                .arena
                .get_mut(slot)
                .expect("reduce called on a live slot");
            node.qty.0 -= fill_qty;
            node.account
        };

        let map = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };
        map.get_mut(&price).expect("level must exist").total_qty.0 -= fill_qty;

        let entry = self
            .accounts
            .get_mut(&account)
            .expect("node's account must have an entry");
        entry.notional -= price.0 as u128 * fill_qty as u128;
    }

    /// Sweep the opposite side against an aggressor at price-time priority,
    /// executing each match at the maker's resting price (SPEC §2).
    ///
    /// Self-trade prevention is applied inline: a resting order owned by
    /// `account_id` is cancelled for real (emitting `Cancelled`) rather
    /// than filled, and does not consume the aggressor's quantity (SPEC
    /// §2, cancel-resting policy). Shared by every order type that can
    /// take liquidity — GTC, IOC, FOK (after its precheck passes), and
    /// Market — and by a loses-priority `modify`, all "subject to STP
    /// identically to a fresh submit."
    ///
    /// Returns the quantity still remaining after the sweep (0 if fully
    /// filled). Callers decide what remaining means for them: GTC/modify
    /// rest it, IOC/FOK/Market discard it.
    fn sweep(
        &mut self,
        account_id: AccountId,
        order_id: OrderId,
        side: Side,
        price: Price,
        qty: Qty,
        emit: &mut dyn FnMut(Event),
    ) -> u64 {
        let opposite_side = match side {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        };
        let mut remaining = qty.0;

        // Sweep while the opposite side still crosses. Price equality
        // counts as crossing (SPEC §2) — an order that would lock the book
        // matches instead of resting.
        'levels: while remaining > 0 {
            let level_price = match side {
                Side::Buy => self.asks.keys().next().copied().filter(|&ask| ask <= price),
                Side::Sell => self
                    .bids
                    .keys()
                    .next_back()
                    .copied()
                    .filter(|&bid| bid >= price),
            };
            let Some(level_price) = level_price else {
                break;
            };

            // Consume this level's FIFO chain, oldest first, until either
            // it's exhausted or the aggressor is filled.
            loop {
                if remaining == 0 {
                    break 'levels;
                }
                let maker_slot = {
                    let map = match opposite_side {
                        Side::Buy => &self.bids,
                        Side::Sell => &self.asks,
                    };
                    match map.get(&level_price) {
                        Some(level) if level.head != NULL => level.head,
                        _ => continue 'levels, // level fully consumed and removed
                    }
                };
                let maker = *self.arena.get(maker_slot).expect("level head must be live");

                if maker.account == account_id {
                    // Self-trade prevention: cancel-resting. The maker is
                    // removed without consuming any of the aggressor's
                    // quantity, and matching continues.
                    self.unlink(maker_slot);
                    emit(Event::Cancelled {
                        account_id: maker.account,
                        order_id: maker.id,
                    });
                    continue;
                }

                let fill_qty = remaining.min(maker.qty.0);
                remaining -= fill_qty;
                // One read, reused for last_trade, both Filled events, and
                // the Trade print below -- these all describe the same
                // execution and must never be able to drift from each
                // other by each recomputing it independently.
                let fill_price = maker.price;
                self.last_trade = Some(fill_price);

                // Execution is always at the maker's resting price, never
                // the taker's limit (SPEC §2).
                emit(Event::Filled {
                    account_id,
                    order_id,
                    side,
                    price: fill_price,
                    qty: Qty(fill_qty),
                    resting_qty: Qty(remaining),
                    state: fill_state(Qty(remaining)),
                });
                let maker_resting_qty = maker.qty.0 - fill_qty;
                emit(Event::Filled {
                    account_id: maker.account,
                    order_id: maker.id,
                    side: opposite_side,
                    price: fill_price,
                    qty: Qty(fill_qty),
                    resting_qty: Qty(maker_resting_qty),
                    state: fill_state(Qty(maker_resting_qty)),
                });
                emit(Event::Trade {
                    price: fill_price,
                    qty: Qty(fill_qty),
                    taker_side: side,
                });

                if fill_qty == maker.qty.0 {
                    self.unlink(maker_slot);
                } else {
                    self.reduce_resting_qty(maker_slot, opposite_side, level_price, fill_qty);
                }
            }
        }

        remaining
    }

    /// Account-aware fillability check for FOK: does the crossing depth
    /// for `side`/`price` contain enough quantity belonging to accounts
    /// *other than* `account_id` to fill `qty` in full?
    ///
    /// Self-owned crossable depth would be cancelled by STP, not filled
    /// (SPEC §2), so it must not count toward fillability — otherwise FOK
    /// could report "fillable" for a book where the only crossing depth is
    /// the aggressor's own, which `sweep` would then cancel rather than
    /// fill, leaving the order under-filled with no way back out.
    ///
    /// Walks only the crossing levels a real match would touch, early-
    /// exiting the moment enough foreign quantity is found — the one
    /// permitted exception to the no-linear-scan rule (SPEC §6), bounded
    /// by exactly what a successful match would touch. Not the hot path.
    fn is_fillable(&self, account_id: AccountId, side: Side, price: Price, qty: Qty) -> bool {
        let mut needed = qty.0;
        match side {
            Side::Buy => {
                for (&level_price, level) in self.asks.iter() {
                    if level_price > price {
                        break;
                    }
                    if self.level_foreign_qty(level, account_id, &mut needed) {
                        return true;
                    }
                }
            }
            Side::Sell => {
                for (&level_price, level) in self.bids.iter().rev() {
                    if level_price < price {
                        break;
                    }
                    if self.level_foreign_qty(level, account_id, &mut needed) {
                        return true;
                    }
                }
            }
        }
        needed == 0
    }

    /// Walk one level's chain, subtracting every node *not* owned by
    /// `account_id` from `needed`. Returns `true` (short-circuiting the
    /// caller's loop) the instant `needed` reaches zero.
    fn level_foreign_qty(&self, level: &Level, account_id: AccountId, needed: &mut u64) -> bool {
        let mut cursor = level.head;
        while cursor != NULL {
            let node = self
                .arena
                .get(cursor)
                .expect("level chain node must be live");
            if node.account != account_id {
                let take = (*needed).min(node.qty.0);
                *needed -= take;
                if *needed == 0 {
                    return true;
                }
            }
            cursor = node.next;
        }
        false
    }

    /// PostOnly's crossing check, evaluated *after* STP would have applied
    /// (SPEC §2's "PostOnly and self-trade prevention ordering"). Walks
    /// the crossing depth for `side`/`price`; a same-account resting order
    /// found along the way is cancelled for real (not merely excluded from
    /// a count, unlike FOK's read-only precheck — PostOnly has no
    /// atomicity constraint forcing a dry run first) and the walk
    /// continues. Stops the instant a *foreign* resting order is found,
    /// without touching it, and returns `true`: PostOnly must never
    /// consume foreign liquidity under any outcome. Returns `false` once
    /// crossing depth is exhausted with no foreign order found.
    ///
    /// Self-cancellations already performed before a foreign order is
    /// found are not reversed if the caller goes on to reject — STP
    /// removing a self-trade risk is correct regardless of what the order
    /// ultimately does.
    fn stp_clear_crossing_self_orders(
        &mut self,
        account_id: AccountId,
        side: Side,
        price: Price,
        emit: &mut dyn FnMut(Event),
    ) -> bool {
        let opposite_side = match side {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        };
        loop {
            let level_price = match side {
                Side::Buy => self.asks.keys().next().copied().filter(|&ask| ask <= price),
                Side::Sell => self
                    .bids
                    .keys()
                    .next_back()
                    .copied()
                    .filter(|&bid| bid >= price),
            };
            let Some(level_price) = level_price else {
                return false;
            };

            let head_slot = {
                let map = match opposite_side {
                    Side::Buy => &self.bids,
                    Side::Sell => &self.asks,
                };
                match map.get(&level_price) {
                    Some(level) if level.head != NULL => level.head,
                    _ => continue, // level fully cleared; re-check crossing at the top
                }
            };
            let node = *self.arena.get(head_slot).expect("level head must be live");

            if node.account == account_id {
                self.unlink(head_slot);
                emit(Event::Cancelled {
                    account_id: node.account,
                    order_id: node.id,
                });
                continue;
            }
            return true;
        }
    }

    /// Submit a GTC limit order: match what it can against the opposite
    /// side, rest any remainder at the back of its level (SPEC §2).
    pub fn submit_gtc(
        &mut self,
        account_id: AccountId,
        order_id: OrderId,
        side: Side,
        price: Price,
        qty: Qty,
        emit: &mut dyn FnMut(Event),
    ) {
        // Internal invariants: the gateway validates both nonzero before a
        // Command ever reaches core (SPEC §3).
        debug_assert_ne!(qty.0, 0, "qty is validated nonzero at the gateway");
        debug_assert_ne!(
            price.0, 0,
            "price is validated nonzero for non-Market orders at the gateway"
        );

        if self.order_index.contains_key(&(account_id, order_id)) {
            emit(Event::Rejected {
                account_id,
                order_id,
                reason: RejectReason::DuplicateOrderId,
            });
            return;
        }

        let remaining = self.sweep(account_id, order_id, side, price, qty, emit);
        if remaining > 0 {
            self.rest(account_id, order_id, side, price, Qty(remaining));
        }
        emit(Event::Accepted {
            account_id,
            order_id,
            resting_qty: Qty(remaining),
        });
    }

    /// Submit an IOC limit order: match what it can, discard the
    /// remainder rather than resting it (SPEC §2).
    pub fn submit_ioc(
        &mut self,
        account_id: AccountId,
        order_id: OrderId,
        side: Side,
        price: Price,
        qty: Qty,
        emit: &mut dyn FnMut(Event),
    ) {
        debug_assert_ne!(qty.0, 0, "qty is validated nonzero at the gateway");
        debug_assert_ne!(
            price.0, 0,
            "price is validated nonzero for non-Market orders at the gateway"
        );

        if self.order_index.contains_key(&(account_id, order_id)) {
            emit(Event::Rejected {
                account_id,
                order_id,
                reason: RejectReason::DuplicateOrderId,
            });
            return;
        }

        self.sweep(account_id, order_id, side, price, qty, emit);
        emit(Event::Accepted {
            account_id,
            order_id,
            resting_qty: Qty(0),
        });
    }

    /// Submit a FOK limit order: fill in full immediately, or reject with
    /// zero fills (SPEC §2). The precheck ([`Book::is_fillable`]) is
    /// account-aware, so a pass here guarantees `sweep` drives `remaining`
    /// to zero.
    pub fn submit_fok(
        &mut self,
        account_id: AccountId,
        order_id: OrderId,
        side: Side,
        price: Price,
        qty: Qty,
        emit: &mut dyn FnMut(Event),
    ) {
        debug_assert_ne!(qty.0, 0, "qty is validated nonzero at the gateway");
        debug_assert_ne!(
            price.0, 0,
            "price is validated nonzero for non-Market orders at the gateway"
        );

        if self.order_index.contains_key(&(account_id, order_id)) {
            emit(Event::Rejected {
                account_id,
                order_id,
                reason: RejectReason::DuplicateOrderId,
            });
            return;
        }

        if !self.is_fillable(account_id, side, price, qty) {
            emit(Event::Rejected {
                account_id,
                order_id,
                reason: RejectReason::NotFullyFillable,
            });
            return;
        }

        let remaining = self.sweep(account_id, order_id, side, price, qty, emit);
        debug_assert_eq!(
            remaining, 0,
            "is_fillable passed, so sweep must have driven remaining to zero"
        );
        emit(Event::Accepted {
            account_id,
            order_id,
            resting_qty: Qty(0),
        });
    }

    /// Submit a Market order: IOC semantics with an unbounded limit —
    /// `Price::MAX` for a buy, `0` for a sell (SPEC §2). Never rejects for
    /// insufficient liquidity: fills whatever depth exists and discards
    /// the remainder, down to zero fills against an empty book.
    pub fn submit_market(
        &mut self,
        account_id: AccountId,
        order_id: OrderId,
        side: Side,
        qty: Qty,
        emit: &mut dyn FnMut(Event),
    ) {
        debug_assert_ne!(qty.0, 0, "qty is validated nonzero at the gateway");

        if self.order_index.contains_key(&(account_id, order_id)) {
            emit(Event::Rejected {
                account_id,
                order_id,
                reason: RejectReason::DuplicateOrderId,
            });
            return;
        }

        let bound = match side {
            Side::Buy => Price(u64::MAX),
            Side::Sell => Price(0),
        };
        self.sweep(account_id, order_id, side, bound, qty, emit);
        emit(Event::Accepted {
            account_id,
            order_id,
            resting_qty: Qty(0),
        });
    }

    /// Submit a PostOnly limit order: reject if it would cross *after*
    /// self-trade prevention; otherwise rest in full (SPEC §2).
    pub fn submit_postonly(
        &mut self,
        account_id: AccountId,
        order_id: OrderId,
        side: Side,
        price: Price,
        qty: Qty,
        emit: &mut dyn FnMut(Event),
    ) {
        debug_assert_ne!(qty.0, 0, "qty is validated nonzero at the gateway");
        debug_assert_ne!(price.0, 0, "price is validated nonzero at the gateway");

        if self.order_index.contains_key(&(account_id, order_id)) {
            emit(Event::Rejected {
                account_id,
                order_id,
                reason: RejectReason::DuplicateOrderId,
            });
            return;
        }

        if self.stp_clear_crossing_self_orders(account_id, side, price, emit) {
            emit(Event::Rejected {
                account_id,
                order_id,
                reason: RejectReason::WouldCross,
            });
            return;
        }

        self.rest(account_id, order_id, side, price, qty);
        emit(Event::Accepted {
            account_id,
            order_id,
            resting_qty: qty,
        });
    }

    /// Amend a resting order's price and/or quantity (SPEC §2).
    ///
    /// A decrease or unchanged quantity at an unchanged price **retains**
    /// queue position: updated in place, never cancel-then-submit, which
    /// would lose it. An increase or a price change **loses** it: the
    /// node is unlinked and re-enters at the back of its (possibly new)
    /// level. A price change re-enters matching as a fresh aggressor,
    /// subject to STP identically to a fresh submit; `Replaced` is always
    /// emitted before any resulting `Filled` events.
    pub fn modify(
        &mut self,
        account_id: AccountId,
        order_id: OrderId,
        new_price: Price,
        new_qty: Qty,
        emit: &mut dyn FnMut(Event),
    ) {
        if new_qty.0 == 0 {
            // A modify to zero is a cancel; clients send CancelOrder for
            // that (SPEC §2).
            emit(Event::Rejected {
                account_id,
                order_id,
                reason: RejectReason::ZeroQuantity,
            });
            return;
        }

        let Some(&slot) = self.order_index.get(&(account_id, order_id)) else {
            // No-oracle rule, identical to cancel: wrong account and
            // unknown id reject the same way (SPEC §2).
            emit(Event::Rejected {
                account_id,
                order_id,
                reason: RejectReason::UnknownOrderId,
            });
            return;
        };
        let node = *self
            .arena
            .get(slot)
            .expect("order index points to a live slot");

        let price_changed = new_price != node.price;
        let qty_increased = new_qty.0 > node.qty.0;

        if !price_changed && !qty_increased {
            let old_qty = node.qty.0;
            self.arena.get_mut(slot).expect("just read above").qty = new_qty;

            let map = match node.side {
                Side::Buy => &mut self.bids,
                Side::Sell => &mut self.asks,
            };
            let level = map
                .get_mut(&node.price)
                .expect("resting node's level must exist");
            level.total_qty.0 = level.total_qty.0 - old_qty + new_qty.0;

            let entry = self
                .accounts
                .get_mut(&node.account)
                .expect("resting node's account must have an entry");
            entry.notional = entry.notional - (node.price.0 as u128 * old_qty as u128)
                + (node.price.0 as u128 * new_qty.0 as u128);

            emit(Event::Replaced {
                account_id,
                order_id,
                new_qty,
                priority_retained: true,
            });
            return;
        }

        // Loses priority either way: unlink first (this is not
        // cancel-then-submit from the caller's perspective -- it's one
        // atomic amend -- but internally it's built on the same `unlink`
        // every other removal goes through). `Replaced` before any
        // `Filled` the re-entry produces (SPEC §2).
        self.unlink(slot);
        emit(Event::Replaced {
            account_id,
            order_id,
            new_qty,
            priority_retained: false,
        });

        // Re-enters matching as a fresh aggressor at the new price. If
        // only qty changed (price unchanged), this can never newly cross
        // -- the level it left was already non-crossing by invariant, and
        // removing an order from one side cannot cross the other side --
        // so the sweep below is a no-op and this simplifies to "unlink,
        // then rest at the back" for that case, and "unlink, sweep, rest
        // the remainder" for a genuinely crossing price change.
        let remaining = self.sweep(account_id, order_id, node.side, new_price, new_qty, emit);
        if remaining > 0 {
            self.rest(account_id, order_id, node.side, new_price, Qty(remaining));
        }
    }

    /// Cancel a resting order. O(1): a single `(AccountId, OrderId)` lookup
    /// plus [`Book::unlink`].
    ///
    /// The no-oracle rule (SPEC §2) falls out of the `(AccountId, OrderId)`
    /// key for free: a cancel from the wrong account simply misses the
    /// index, identically to a cancel for an id that was never used — same
    /// reason code, same event shape, no separate ownership check to get
    /// wrong.
    pub fn cancel(
        &mut self,
        account_id: AccountId,
        order_id: OrderId,
        emit: &mut dyn FnMut(Event),
    ) {
        match self.order_index.get(&(account_id, order_id)) {
            Some(&slot) => {
                self.unlink(slot);
                emit(Event::Cancelled {
                    account_id,
                    order_id,
                });
            }
            None => {
                emit(Event::Rejected {
                    account_id,
                    order_id,
                    reason: RejectReason::UnknownOrderId,
                });
            }
        }
    }

    /// Cancel every resting order for one account, emitting one
    /// `Cancelled` per order. A no-op, not a rejection, for an account
    /// with no resting orders — including one the book has never seen
    /// (SPEC §2).
    ///
    /// Emission order is a snapshot of the account's slot array taken
    /// before any of them are unlinked: the array's current traversal
    /// order, which is deterministic but not necessarily arrival order —
    /// prior individual cancels may already have reordered it via
    /// `swap_remove` (SPEC §2, §4).
    ///
    /// This walks the snapshot and calls [`Book::unlink`] once per order,
    /// rather than the "walk then bulk-clear" shape SPEC §4 describes,
    /// specifically so the account's `slots` array is still only ever
    /// touched from `rest`/`unlink` — never a third place. Calling
    /// `unlink` from a loop driven by an independent snapshot is safe:
    /// each call's `swap_remove` acts on whatever the live array's
    /// *current* state is, which is exactly what the next iteration
    /// needs regardless of how earlier iterations shrank it.
    ///
    /// The snapshot lives in `mass_cancel_scratch`, reused across calls
    /// rather than a fresh `Vec` cloned from `entry.slots` each time
    /// (found allocating by `hot_path_allocates_nothing`, SPEC §6/§9):
    /// `extend_from_slice` after `clear` ends the immutable borrow on
    /// `self.accounts` the same way the clone did, without allocating
    /// once the buffer has grown to cover the largest mass-cancel this
    /// `Book` has handled so far.
    pub fn mass_cancel(&mut self, account_id: AccountId, emit: &mut dyn FnMut(Event)) {
        let Some(entry) = self.accounts.get(&account_id) else {
            return;
        };
        self.mass_cancel_scratch.clear();
        self.mass_cancel_scratch.extend_from_slice(&entry.slots);

        for i in 0..self.mass_cancel_scratch.len() {
            let slot = self.mass_cancel_scratch[i];
            let order_id = self
                .arena
                .get(slot)
                .expect("snapshot slot must still be live")
                .id;
            self.unlink(slot);
            emit(Event::Cancelled {
                account_id,
                order_id,
            });
        }
    }

    /// Dumps current book state for inspection (SPEC §3's `Snapshot`
    /// command): per-side, per-price-level resting quantity and order
    /// count; best bid/ask; `last_trade`; and per-account open-order count
    /// and notional. Read-only -- never mutates, never rejected by risk,
    /// inert under the kill switch's drain (SPEC §5).
    ///
    /// Reuses `assert_invariants()`'s own traversal shape (`for (&price,
    /// level) in &self.bids` then `&self.asks`, the same deterministic
    /// `BTreeMap` order, then `walk_level` to discover each level's
    /// individual orders) rather than a second walker. `self.accounts` (a
    /// `HashMap`) is never iterated directly -- the per-account section
    /// below is ordered by first appearance during this same level walk,
    /// with a `HashSet` used only for membership checking, never for
    /// output order, so no `HashMap` iteration order reaches the wire
    /// (CLAUDE.md).
    ///
    /// This performs one allocating pass over the book (the account-
    /// tracking `Vec`/`HashSet`) every time it's called, unlike
    /// `submit`/`cancel`/`modify`, and that's accepted, not a gap:
    /// `Snapshot` is an operator-invoked inspection command, not
    /// client-invoked order flow, so it isn't held to the zero-allocation
    /// hot-path standard that flow is (SPEC §6/§9) -- what makes
    /// something "hot path" here is whether it's on the per-order flow,
    /// not whether it happens to run on the matching thread. The
    /// allocation is bounded by the size of the book at the moment it's
    /// called, the same way `mass_cancel`'s now-fixed clone was bounded by
    /// one account's resting-order count -- but `mass_cancel` is ordinary
    /// client order flow, and `Snapshot` is not, which is the actual line
    /// (see `hot_path_allocates_nothing`, `crates/risk/tests/zero_alloc.rs`,
    /// which deliberately excludes `Snapshot` from its measured sequence
    /// for exactly this reason).
    pub fn snapshot(&self, emit: &mut dyn FnMut(Event)) {
        let mut accounts_seen: HashSet<AccountId> = HashSet::new();
        let mut accounts_order: Vec<AccountId> = Vec::new();
        let mut level_count: u64 = 0;

        for (side, map) in [(Side::Buy, &self.bids), (Side::Sell, &self.asks)] {
            for (&price, level) in map {
                emit(Event::SnapshotLevel {
                    side,
                    price,
                    qty: level.total_qty(),
                    order_count: u64::from(level.count()),
                });
                level_count += 1;

                for slot in self.walk_level(side, price, level) {
                    let node = self
                        .arena
                        .get(slot)
                        .expect("slot just returned by walk_level must be live");
                    if accounts_seen.insert(node.account) {
                        accounts_order.push(node.account);
                    }
                }
            }
        }

        for account_id in &accounts_order {
            let entry = self
                .accounts
                .get(account_id)
                .expect("account discovered via a resting order must have an AccountEntry");
            emit(Event::SnapshotAccount {
                account_id: *account_id,
                open_order_count: entry.open_order_count() as u64,
                notional: entry.notional(),
            });
        }

        let (best_bid, best_ask) = self.top_of_book();
        emit(Event::SnapshotSummary {
            best_bid,
            best_ask,
            last_trade: self.last_trade,
            level_count,
            account_count: accounts_order.len() as u64,
        });
    }

    /// Walk one level's chain from head to tail, checking link consistency,
    /// tail reachability, node/map-key agreement, non-zero qty/price, and
    /// the cached `total_qty`/`count` along the way. Returns the slots
    /// visited, in order, so the caller can cross-check them against the
    /// order and account indexes.
    fn walk_level(&self, side: Side, price: Price, level: &Level) -> Vec<u32> {
        let mut slots = Vec::new();
        let mut total: u64 = 0;
        let mut prev_slot = NULL;
        let mut cursor = level.head;

        while cursor != NULL {
            let node = self.arena.get(cursor).unwrap_or_else(|| {
                panic!("level chain at {price:?} points to freed slot {cursor}")
            });

            assert_eq!(
                node.prev, prev_slot,
                "back-link mismatch in level at {price:?}, slot {cursor}"
            );
            assert_eq!(
                node.price, price,
                "node at slot {cursor} filed under wrong price"
            );
            assert_eq!(node.side, side, "node at slot {cursor} filed on wrong side");
            assert_ne!(
                node.qty.0, 0,
                "resting node at slot {cursor} has zero quantity"
            );
            assert_ne!(
                node.price.0, 0,
                "resting node at slot {cursor} has zero price"
            );

            slots.push(cursor);
            total += node.qty.0;
            prev_slot = cursor;
            cursor = node.next;
        }

        assert_eq!(
            prev_slot, level.tail,
            "level at {price:?} did not walk to its cached tail"
        );
        assert_eq!(
            slots.len() as u32,
            level.count,
            "level at {price:?} count mismatch"
        );
        assert_eq!(
            total, level.total_qty.0,
            "level at {price:?} total_qty mismatch"
        );

        slots
    }

    /// Checks every bullet in SPEC §6's `assert_invariants()` list. Written
    /// before the matching logic it will guard — tests exercise it against
    /// hand-built fixtures until stage 1 part two gives it real operations
    /// to run after.
    pub fn assert_invariants(&self) {
        // No empty levels remain in either price map.
        for (price, level) in &self.bids {
            assert!(!level.is_empty(), "empty bid level remains at {price:?}");
        }
        for (price, level) in &self.asks {
            assert!(!level.is_empty(), "empty ask level remains at {price:?}");
        }

        // Walk every level: validates cached total_qty/count, forward and
        // backward link consistency, tail reachability, node
        // side/price-vs-map-key agreement, and non-zero qty/price, all in
        // one pass. Also collects every slot reachable from some level, for
        // the order-index and account-index cross-checks below.
        let mut found_in_levels: HashSet<u32> = HashSet::new();
        for (&price, level) in &self.bids {
            for slot in self.walk_level(Side::Buy, price, level) {
                let inserted = found_in_levels.insert(slot);
                assert!(inserted, "slot {slot} reachable from more than one level");
            }
        }
        for (&price, level) in &self.asks {
            for slot in self.walk_level(Side::Sell, price, level) {
                let inserted = found_in_levels.insert(slot);
                assert!(inserted, "slot {slot} reachable from more than one level");
            }
        }

        // Every live arena slot is reachable from exactly one level — no
        // orphan nodes sitting in the arena unfiled.
        assert_eq!(
            self.arena.len(),
            found_in_levels.len(),
            "arena has slots not reachable from any level"
        );

        // Order index and book agree in both directions.
        assert_eq!(
            self.order_index.len(),
            found_in_levels.len(),
            "order index size does not match reachable slot count"
        );
        for (&(account, order_id), &slot) in &self.order_index {
            assert!(
                found_in_levels.contains(&slot),
                "order index entry points to a slot not reachable from any level"
            );
            let node = self
                .arena
                .get(slot)
                .unwrap_or_else(|| panic!("order index points to freed slot {slot}"));
            assert_eq!(node.id, order_id, "order index key order_id mismatch");
            assert_eq!(node.account, account, "order index key account mismatch");
        }
        for &slot in &found_in_levels {
            let node = self
                .arena
                .get(slot)
                .expect("slot reachable from a level must be live");
            assert_eq!(
                self.order_index.get(&(node.account, node.id)),
                Some(&slot),
                "node has no matching order index entry"
            );
        }

        // Account index agrees with the book in both directions.
        for &slot in &found_in_levels {
            let node = self
                .arena
                .get(slot)
                .expect("slot reachable from a level must be live");
            let entry = self
                .accounts
                .get(&node.account)
                .unwrap_or_else(|| panic!("node at slot {slot} has no AccountEntry"));
            assert_eq!(
                entry.slots.get(node.acct_idx as usize),
                Some(&slot),
                "node's acct_idx does not point back to its own slot"
            );
        }
        for (&account, entry) in &self.accounts {
            let mut walked_notional: u128 = 0;
            for (idx, &slot) in entry.slots.iter().enumerate() {
                let node = self
                    .arena
                    .get(slot)
                    .unwrap_or_else(|| panic!("account slots[{idx}] points to freed slot {slot}"));
                assert_eq!(
                    node.account, account,
                    "account slots entry owned by wrong account"
                );
                assert_eq!(
                    node.acct_idx as usize, idx,
                    "account slots entry acct_idx mismatch"
                );
                walked_notional += node.price.0 as u128 * node.qty.0 as u128;
            }
            assert_eq!(
                walked_notional, entry.notional,
                "account notional cache mismatch"
            );
        }

        // The book is neither crossed nor locked.
        if let (Some(bid), Some(ask)) = (self.best_bid(), self.best_ask()) {
            assert!(
                bid < ask,
                "book is crossed or locked: best bid {bid:?} >= best ask {ask:?}"
            );
        }
    }
}

impl Default for Book {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::Node;
    use crate::types::Qty;
    use proptest::prelude::*;

    /// One resting buy: account 1, order 1, 5 lots at price 100 tick, in
    /// arena slot 0. Every index (order, account, level) built by hand and
    /// mutually consistent — this is the fixture every "invariant catches
    /// X" test below starts from and then deliberately breaks one thing in.
    fn book_with_one_resting_order() -> Book {
        let mut book = Book::new();

        book.arena.slots.push(Some(Node {
            id: OrderId(1),
            account: AccountId(1),
            price: Price(100),
            qty: Qty(5),
            side: Side::Buy,
            prev: NULL,
            next: NULL,
            acct_idx: 0,
        }));
        let slot = 0u32;

        let level = Level {
            head: slot,
            tail: slot,
            count: 1,
            total_qty: Qty(5),
        };
        book.bids.insert(Price(100), level);

        book.order_index.insert((AccountId(1), OrderId(1)), slot);

        let mut entry = AccountEntry::default();
        entry.slots.push(slot);
        entry.notional = 100u128 * 5u128;
        book.accounts.insert(AccountId(1), entry);

        book
    }

    fn submit(
        book: &mut Book,
        account: u64,
        order: u64,
        side: Side,
        price: u64,
        qty: u64,
    ) -> Vec<Event> {
        let mut out = Vec::new();
        book.submit_gtc(
            AccountId(account),
            OrderId(order),
            side,
            Price(price),
            Qty(qty),
            &mut |e| out.push(e),
        );
        out
    }

    fn cancel(book: &mut Book, account: u64, order: u64) -> Vec<Event> {
        let mut out = Vec::new();
        book.cancel(AccountId(account), OrderId(order), &mut |e| out.push(e));
        out
    }

    fn mass_cancel(book: &mut Book, account: u64) -> Vec<Event> {
        let mut out = Vec::new();
        book.mass_cancel(AccountId(account), &mut |e| out.push(e));
        out
    }

    fn submit_ioc(
        book: &mut Book,
        account: u64,
        order: u64,
        side: Side,
        price: u64,
        qty: u64,
    ) -> Vec<Event> {
        let mut out = Vec::new();
        book.submit_ioc(
            AccountId(account),
            OrderId(order),
            side,
            Price(price),
            Qty(qty),
            &mut |e| out.push(e),
        );
        out
    }

    fn submit_fok(
        book: &mut Book,
        account: u64,
        order: u64,
        side: Side,
        price: u64,
        qty: u64,
    ) -> Vec<Event> {
        let mut out = Vec::new();
        book.submit_fok(
            AccountId(account),
            OrderId(order),
            side,
            Price(price),
            Qty(qty),
            &mut |e| out.push(e),
        );
        out
    }

    fn submit_market(
        book: &mut Book,
        account: u64,
        order: u64,
        side: Side,
        qty: u64,
    ) -> Vec<Event> {
        let mut out = Vec::new();
        book.submit_market(
            AccountId(account),
            OrderId(order),
            side,
            Qty(qty),
            &mut |e| out.push(e),
        );
        out
    }

    fn submit_postonly(
        book: &mut Book,
        account: u64,
        order: u64,
        side: Side,
        price: u64,
        qty: u64,
    ) -> Vec<Event> {
        let mut out = Vec::new();
        book.submit_postonly(
            AccountId(account),
            OrderId(order),
            side,
            Price(price),
            Qty(qty),
            &mut |e| out.push(e),
        );
        out
    }

    fn modify(
        book: &mut Book,
        account: u64,
        order: u64,
        new_price: u64,
        new_qty: u64,
    ) -> Vec<Event> {
        let mut out = Vec::new();
        book.modify(
            AccountId(account),
            OrderId(order),
            Price(new_price),
            Qty(new_qty),
            &mut |e| out.push(e),
        );
        out
    }

    #[test]
    fn empty_book_satisfies_invariants() {
        Book::new().assert_invariants();
    }

    #[test]
    fn single_resting_order_satisfies_invariants() {
        book_with_one_resting_order().assert_invariants();
    }

    #[test]
    #[should_panic(expected = "empty bid level remains")]
    fn empty_level_left_in_map_is_caught() {
        let mut book = Book::new();
        book.bids.insert(Price(100), Level::default());
        book.assert_invariants();
    }

    #[test]
    #[should_panic(expected = "count mismatch")]
    fn wrong_cached_count_is_caught() {
        let mut book = book_with_one_resting_order();
        book.bids.get_mut(&Price(100)).unwrap().count = 2;
        book.assert_invariants();
    }

    #[test]
    #[should_panic(expected = "total_qty mismatch")]
    fn wrong_cached_total_qty_is_caught() {
        let mut book = book_with_one_resting_order();
        book.bids.get_mut(&Price(100)).unwrap().total_qty = Qty(999);
        book.assert_invariants();
    }

    #[test]
    #[should_panic(expected = "not reachable from any level")]
    fn orphan_arena_node_is_caught() {
        let mut book = book_with_one_resting_order();
        // A second node that exists in the arena but was never linked into
        // any level's chain.
        book.arena.slots.push(Some(Node {
            id: OrderId(2),
            account: AccountId(1),
            price: Price(100),
            qty: Qty(1),
            side: Side::Buy,
            prev: NULL,
            next: NULL,
            acct_idx: 1,
        }));
        book.assert_invariants();
    }

    #[test]
    #[should_panic(expected = "order index size does not match")]
    fn missing_order_index_entry_is_caught() {
        let mut book = book_with_one_resting_order();
        book.order_index.clear();
        book.assert_invariants();
    }

    #[test]
    #[should_panic(expected = "acct_idx does not point back")]
    fn wrong_acct_idx_is_caught() {
        let mut book = book_with_one_resting_order();
        book.arena.get_mut(0).unwrap().acct_idx = 7;
        book.assert_invariants();
    }

    #[test]
    #[should_panic(expected = "notional cache mismatch")]
    fn wrong_account_notional_is_caught() {
        let mut book = book_with_one_resting_order();
        book.accounts.get_mut(&AccountId(1)).unwrap().notional = 999;
        book.assert_invariants();
    }

    #[test]
    #[should_panic(expected = "crossed or locked")]
    fn crossed_book_is_caught() {
        let mut book = book_with_one_resting_order();
        // A fully self-consistent resting ask at 99, priced under the
        // existing bid at 100 — real matching would have crossed instead
        // of resting either order (SPEC §2), but assert_invariants() must
        // catch it structurally regardless of how it came to exist.
        book.arena.slots.push(Some(Node {
            id: OrderId(2),
            account: AccountId(2),
            price: Price(99),
            qty: Qty(3),
            side: Side::Sell,
            prev: NULL,
            next: NULL,
            acct_idx: 0,
        }));
        let slot = 1u32;

        let level = Level {
            head: slot,
            tail: slot,
            count: 1,
            total_qty: Qty(3),
        };
        book.asks.insert(Price(99), level);

        book.order_index.insert((AccountId(2), OrderId(2)), slot);

        let mut entry = AccountEntry::default();
        entry.slots.push(slot);
        entry.notional = 99u128 * 3u128;
        book.accounts.insert(AccountId(2), entry);

        book.assert_invariants();
    }

    // -- submit_gtc / cancel / mass_cancel scenario tests --------------

    #[test]
    fn submit_with_no_crossing_rests_in_full() {
        let mut book = Book::new();
        let events = submit(&mut book, 1, 1, Side::Buy, 100, 5);
        assert_eq!(
            events,
            vec![Event::Accepted {
                account_id: AccountId(1),
                order_id: OrderId(1),
                resting_qty: Qty(5),
            }]
        );
        book.assert_invariants();
        assert_eq!(book.best_bid(), Some(Price(100)));
    }

    #[test]
    fn fifo_within_a_level() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 2);
        submit(&mut book, 2, 2, Side::Sell, 100, 2);
        submit(&mut book, 3, 3, Side::Sell, 100, 2);
        book.assert_invariants();

        // Aggressor qty 5 against three 2-lot makers: fully fills the
        // first two (submitted first), partially fills the third.
        let events = submit(&mut book, 9, 100, Side::Buy, 100, 5);

        assert_eq!(
            events,
            vec![
                Event::Filled {
                    account_id: AccountId(9),
                    order_id: OrderId(100),
                    side: Side::Buy,
                    price: Price(100),
                    qty: Qty(2),
                    resting_qty: Qty(3),
                    state: FillState::PartiallyFilled,
                },
                Event::Filled {
                    account_id: AccountId(1),
                    order_id: OrderId(1),
                    side: Side::Sell,
                    price: Price(100),
                    qty: Qty(2),
                    resting_qty: Qty(0),
                    state: FillState::Filled,
                },
                Event::Trade {
                    price: Price(100),
                    qty: Qty(2),
                    taker_side: Side::Buy,
                },
                Event::Filled {
                    account_id: AccountId(9),
                    order_id: OrderId(100),
                    side: Side::Buy,
                    price: Price(100),
                    qty: Qty(2),
                    resting_qty: Qty(1),
                    state: FillState::PartiallyFilled,
                },
                Event::Filled {
                    account_id: AccountId(2),
                    order_id: OrderId(2),
                    side: Side::Sell,
                    price: Price(100),
                    qty: Qty(2),
                    resting_qty: Qty(0),
                    state: FillState::Filled,
                },
                Event::Trade {
                    price: Price(100),
                    qty: Qty(2),
                    taker_side: Side::Buy,
                },
                Event::Filled {
                    account_id: AccountId(9),
                    order_id: OrderId(100),
                    side: Side::Buy,
                    price: Price(100),
                    qty: Qty(1),
                    resting_qty: Qty(0),
                    state: FillState::Filled,
                },
                Event::Filled {
                    account_id: AccountId(3),
                    order_id: OrderId(3),
                    side: Side::Sell,
                    price: Price(100),
                    qty: Qty(1),
                    resting_qty: Qty(1),
                    state: FillState::PartiallyFilled,
                },
                Event::Trade {
                    price: Price(100),
                    qty: Qty(1),
                    taker_side: Side::Buy,
                },
                Event::Accepted {
                    account_id: AccountId(9),
                    order_id: OrderId(100),
                    resting_qty: Qty(0),
                },
            ]
        );
        book.assert_invariants();
        // Maker 3 is still resting with 1 lot, now at the head.
        assert_eq!(book.best_ask(), Some(Price(100)));
    }

    #[test]
    fn sweep_across_levels() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 3);
        submit(&mut book, 2, 2, Side::Sell, 101, 3);
        submit(&mut book, 3, 3, Side::Sell, 102, 3);
        book.assert_invariants();

        let events = submit(&mut book, 9, 100, Side::Buy, 102, 7);

        let maker_fills: Vec<&Event> = events
            .iter()
            .filter(
                |e| matches!(e, Event::Filled { account_id, .. } if *account_id != AccountId(9)),
            )
            .collect();
        // Consumed in ascending price order (price priority): 100 and 101
        // fully, 102 partially.
        assert_eq!(
            maker_fills,
            vec![
                &Event::Filled {
                    account_id: AccountId(1),
                    order_id: OrderId(1),
                    side: Side::Sell,
                    price: Price(100),
                    qty: Qty(3),
                    resting_qty: Qty(0),
                    state: FillState::Filled,
                },
                &Event::Filled {
                    account_id: AccountId(2),
                    order_id: OrderId(2),
                    side: Side::Sell,
                    price: Price(101),
                    qty: Qty(3),
                    resting_qty: Qty(0),
                    state: FillState::Filled,
                },
                &Event::Filled {
                    account_id: AccountId(3),
                    order_id: OrderId(3),
                    side: Side::Sell,
                    price: Price(102),
                    qty: Qty(1),
                    resting_qty: Qty(2),
                    state: FillState::PartiallyFilled,
                },
            ]
        );
        assert_eq!(
            events.last(),
            Some(&Event::Accepted {
                account_id: AccountId(9),
                order_id: OrderId(100),
                resting_qty: Qty(0),
            })
        );
        book.assert_invariants();
        assert_eq!(book.best_ask(), Some(Price(102)));
    }

    #[test]
    fn maker_price_execution() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        // Taker's limit (105) is far more aggressive than the maker's
        // resting price (100) — execution must happen at 100.
        let events = submit(&mut book, 2, 2, Side::Buy, 105, 5);

        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Filled { price, .. } if *price == Price(100)))
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::Filled { price, .. } if *price == Price(105)))
        );
        book.assert_invariants();
    }

    #[test]
    fn partial_fill_rests_remainder() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 3);
        let events = submit(&mut book, 2, 2, Side::Buy, 100, 10);

        assert_eq!(
            events.last(),
            Some(&Event::Accepted {
                account_id: AccountId(2),
                order_id: OrderId(2),
                resting_qty: Qty(7),
            })
        );
        book.assert_invariants();
        assert_eq!(book.best_ask(), None);
        assert_eq!(book.best_bid(), Some(Price(100)));
    }

    #[test]
    fn cancel_from_head_of_level() {
        let mut book = Book::new();
        submit(&mut book, 10, 1, Side::Sell, 100, 2);
        submit(&mut book, 11, 2, Side::Sell, 100, 2);
        submit(&mut book, 12, 3, Side::Sell, 100, 2);
        book.assert_invariants();

        let events = cancel(&mut book, 10, 1);
        assert_eq!(
            events,
            vec![Event::Cancelled {
                account_id: AccountId(10),
                order_id: OrderId(1)
            }]
        );
        book.assert_invariants();

        assert_eq!(
            surviving_maker_order(&mut book),
            vec![OrderId(2), OrderId(3)]
        );
    }

    #[test]
    fn cancel_from_middle_of_level() {
        let mut book = Book::new();
        submit(&mut book, 10, 1, Side::Sell, 100, 2);
        submit(&mut book, 11, 2, Side::Sell, 100, 2);
        submit(&mut book, 12, 3, Side::Sell, 100, 2);
        book.assert_invariants();

        let events = cancel(&mut book, 11, 2);
        assert_eq!(
            events,
            vec![Event::Cancelled {
                account_id: AccountId(11),
                order_id: OrderId(2)
            }]
        );
        book.assert_invariants();

        assert_eq!(
            surviving_maker_order(&mut book),
            vec![OrderId(1), OrderId(3)]
        );
    }

    #[test]
    fn cancel_from_tail_of_level() {
        let mut book = Book::new();
        submit(&mut book, 10, 1, Side::Sell, 100, 2);
        submit(&mut book, 11, 2, Side::Sell, 100, 2);
        submit(&mut book, 12, 3, Side::Sell, 100, 2);
        book.assert_invariants();

        let events = cancel(&mut book, 12, 3);
        assert_eq!(
            events,
            vec![Event::Cancelled {
                account_id: AccountId(12),
                order_id: OrderId(3)
            }]
        );
        book.assert_invariants();

        assert_eq!(
            surviving_maker_order(&mut book),
            vec![OrderId(1), OrderId(2)]
        );
    }

    /// Sweeps whatever rests at price 100 with a fresh aggressor from a
    /// throwaway account, and returns the maker `OrderId`s in the order
    /// they filled — i.e. the level's actual current FIFO order.
    fn surviving_maker_order(book: &mut Book) -> Vec<OrderId> {
        let events = submit(book, 999, 999, Side::Buy, 100, 4);
        events
            .iter()
            .filter_map(|e| match e {
                Event::Filled {
                    account_id,
                    order_id,
                    ..
                } if *account_id != AccountId(999) => Some(*order_id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn cancel_empties_a_level() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        book.assert_invariants();
        assert_eq!(book.best_ask(), Some(Price(100)));

        cancel(&mut book, 1, 1);
        book.assert_invariants();
        assert_eq!(book.best_ask(), None);
    }

    #[test]
    fn cancel_wrong_account_and_unknown_id_reject_identically() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        book.assert_invariants();

        // Account 2 tries to cancel account 1's order.
        let wrong_account = cancel(&mut book, 2, 1);
        // Account 2 tries to cancel an order id nobody has ever used.
        let unknown_id = cancel(&mut book, 2, 999);

        assert_eq!(
            wrong_account,
            vec![Event::Rejected {
                account_id: AccountId(2),
                order_id: OrderId(1),
                reason: RejectReason::UnknownOrderId,
            }]
        );
        assert_eq!(
            unknown_id,
            vec![Event::Rejected {
                account_id: AccountId(2),
                order_id: OrderId(999),
                reason: RejectReason::UnknownOrderId,
            }]
        );
        // Same reason code, same event shape either way -- the no-oracle
        // rule (SPEC §2): a wrong-account cancel cannot be distinguished
        // from a cancel for an id nobody ever used.

        // And critically, account 1's order is untouched.
        book.assert_invariants();
        assert_eq!(book.best_ask(), Some(Price(100)));
    }

    #[test]
    fn cancel_middle_then_mass_cancel_emits_both_survivors() {
        let mut book = Book::new();
        // Three resting orders for the same account, each at its own
        // price -- keeps this test about the account index, not level
        // FIFO.
        submit(&mut book, 1, 1, Side::Sell, 100, 1);
        submit(&mut book, 1, 2, Side::Sell, 101, 1);
        submit(&mut book, 1, 3, Side::Sell, 102, 1);
        book.assert_invariants();

        // Cancel the middle one. Internally: entry.slots was
        // [slot0, slot1, slot2] (acct_idx 0, 1, 2); swap_remove(1) moves
        // slot2 into index 1 and must fix that node's acct_idx to 1. If
        // that fixup is wrong, this assert_invariants() or the
        // mass_cancel below will catch it.
        let cancel_events = cancel(&mut book, 1, 2);
        assert_eq!(
            cancel_events,
            vec![Event::Cancelled {
                account_id: AccountId(1),
                order_id: OrderId(2)
            }]
        );
        book.assert_invariants();

        // Mass-cancel the rest. Emission order follows the slots array's
        // current order after the swap_remove fixup: order 1 (still at
        // index 0), then order 3 (moved into index 1) -- not order 2,
        // which is already gone. This is the swap_remove back-index
        // fixup test.
        let mass_events = mass_cancel(&mut book, 1);
        assert_eq!(
            mass_events,
            vec![
                Event::Cancelled {
                    account_id: AccountId(1),
                    order_id: OrderId(1)
                },
                Event::Cancelled {
                    account_id: AccountId(1),
                    order_id: OrderId(3)
                },
            ]
        );
        book.assert_invariants();
        assert_eq!(book.best_ask(), None);
    }

    #[test]
    fn mass_cancel_on_account_with_no_orders_is_a_noop() {
        let mut book = Book::new();
        // Account never seen at all.
        let events = mass_cancel(&mut book, 42);
        assert!(events.is_empty());
        book.assert_invariants();

        // Account seen (has an AccountEntry from a prior rest) but
        // currently has zero resting orders.
        submit(&mut book, 7, 1, Side::Sell, 100, 5);
        cancel(&mut book, 7, 1);
        let events = mass_cancel(&mut book, 7);
        assert!(events.is_empty());
        book.assert_invariants();
    }

    #[test]
    fn mass_cancel_leaves_other_accounts_untouched() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 3);
        submit(&mut book, 2, 2, Side::Sell, 101, 3);
        book.assert_invariants();

        let events = mass_cancel(&mut book, 1);
        assert_eq!(
            events,
            vec![Event::Cancelled {
                account_id: AccountId(1),
                order_id: OrderId(1)
            }]
        );
        book.assert_invariants();

        // Account 2's order is still resting and still only cancellable
        // by account 2.
        assert_eq!(book.best_ask(), Some(Price(101)));
        let wrong = cancel(&mut book, 1, 2);
        assert_eq!(
            wrong,
            vec![Event::Rejected {
                account_id: AccountId(1),
                order_id: OrderId(2),
                reason: RejectReason::UnknownOrderId,
            }]
        );
        book.assert_invariants();
    }

    #[test]
    fn notional_updates_on_rest_cancel_and_fill() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        assert_eq!(book.accounts.get(&AccountId(1)).unwrap().notional, 500);
        book.assert_invariants();

        // Partial fill: notional drops by exactly the filled portion.
        submit(&mut book, 2, 2, Side::Buy, 100, 2);
        assert_eq!(book.accounts.get(&AccountId(1)).unwrap().notional, 300);
        book.assert_invariants();

        // Cancel the remainder: notional back to zero.
        cancel(&mut book, 1, 1);
        assert_eq!(book.accounts.get(&AccountId(1)).unwrap().notional, 0);
        book.assert_invariants();
    }

    #[test]
    fn duplicate_order_id_for_same_account_is_rejected() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        let events = submit(&mut book, 1, 1, Side::Sell, 101, 3);
        assert_eq!(
            events,
            vec![Event::Rejected {
                account_id: AccountId(1),
                order_id: OrderId(1),
                reason: RejectReason::DuplicateOrderId,
            }]
        );
        book.assert_invariants();
        // Original order untouched -- still at its original price.
        assert_eq!(book.best_ask(), Some(Price(100)));
    }

    #[test]
    fn same_order_id_across_different_accounts_both_succeed_independently() {
        let mut book = Book::new();
        // Two different accounts, each submitting OrderId(1) -- legal per
        // SPEC §2, since OrderId is unique only per account. A bare
        // HashMap<OrderId, u32> order index cannot hold both: the second
        // insert would silently overwrite the first account's entry. This
        // is exactly the bug the (AccountId, OrderId) key exists to
        // prevent, and the single most important test in this stage.
        let account1_events = submit(&mut book, 1, 1, Side::Sell, 100, 5);
        let account2_events = submit(&mut book, 2, 1, Side::Buy, 90, 3);

        assert_eq!(
            account1_events,
            vec![Event::Accepted {
                account_id: AccountId(1),
                order_id: OrderId(1),
                resting_qty: Qty(5),
            }]
        );
        assert_eq!(
            account2_events,
            vec![Event::Accepted {
                account_id: AccountId(2),
                order_id: OrderId(1),
                resting_qty: Qty(3),
            }]
        );
        book.assert_invariants();

        // Both are genuinely resting, as two distinct orders.
        assert_eq!(book.best_ask(), Some(Price(100)));
        assert_eq!(book.best_bid(), Some(Price(90)));

        // Cancelling account 1's order 1 must not touch account 2's order 1.
        let cancel1 = cancel(&mut book, 1, 1);
        assert_eq!(
            cancel1,
            vec![Event::Cancelled {
                account_id: AccountId(1),
                order_id: OrderId(1),
            }]
        );
        book.assert_invariants();
        assert_eq!(book.best_ask(), None);
        assert_eq!(book.best_bid(), Some(Price(90)));

        // Account 2's order 1 is still there, and still only cancellable
        // by account 2.
        let cancel2 = cancel(&mut book, 2, 1);
        assert_eq!(
            cancel2,
            vec![Event::Cancelled {
                account_id: AccountId(2),
                order_id: OrderId(1),
            }]
        );
        book.assert_invariants();
        assert_eq!(book.best_bid(), None);
    }

    // -- IOC --------------------------------------------------------------

    #[test]
    fn ioc_discards_remainder() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 3);
        let events = submit_ioc(&mut book, 2, 2, Side::Buy, 100, 10);

        assert_eq!(
            events.last(),
            Some(&Event::Accepted {
                account_id: AccountId(2),
                order_id: OrderId(2),
                resting_qty: Qty(0),
            })
        );
        book.assert_invariants();
        // Filled 3, discarded the other 7 -- nothing rests on either side.
        assert_eq!(book.best_ask(), None);
        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn ioc_with_no_crossing_discards_everything() {
        let mut book = Book::new();
        let events = submit_ioc(&mut book, 1, 1, Side::Buy, 100, 5);
        assert_eq!(
            events,
            vec![Event::Accepted {
                account_id: AccountId(1),
                order_id: OrderId(1),
                resting_qty: Qty(0),
            }]
        );
        book.assert_invariants();
        assert_eq!(book.best_bid(), None);
    }

    // -- FOK ----------------------------------------------------------------

    #[test]
    fn fok_all_or_nothing_fills_when_fully_fillable() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        let events = submit_fok(&mut book, 2, 2, Side::Buy, 100, 5);

        assert_eq!(
            events.last(),
            Some(&Event::Accepted {
                account_id: AccountId(2),
                order_id: OrderId(2),
                resting_qty: Qty(0),
            })
        );
        book.assert_invariants();
        assert_eq!(book.best_ask(), None);
    }

    #[test]
    fn fok_rejects_when_not_fully_fillable() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 3);
        let events = submit_fok(&mut book, 2, 2, Side::Buy, 100, 5);

        assert_eq!(
            events,
            vec![Event::Rejected {
                account_id: AccountId(2),
                order_id: OrderId(2),
                reason: RejectReason::NotFullyFillable,
            }]
        );
        book.assert_invariants();
        // Zero fills means zero mutations: the resting order is untouched.
        assert_eq!(book.best_ask(), Some(Price(100)));
    }

    #[test]
    fn fok_rejects_when_all_crossable_depth_is_the_aggressor_own() {
        let mut book = Book::new();
        // Account 1's own resting ask is the only crossable depth.
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        let events = submit_fok(&mut book, 1, 2, Side::Buy, 100, 5);

        assert_eq!(
            events,
            vec![Event::Rejected {
                account_id: AccountId(1),
                order_id: OrderId(2),
                reason: RejectReason::NotFullyFillable,
            }]
        );
        book.assert_invariants();
        // The precheck is read-only: a rejected FOK performs zero
        // mutations, so STP never runs and the self-order is untouched.
        assert_eq!(book.best_ask(), Some(Price(100)));
    }

    #[test]
    fn fok_precheck_counts_only_foreign_depth_behind_self_depth() {
        let mut book = Book::new();
        // FIFO at 100: account 1's own order first, then a foreign order
        // with exactly enough quantity on its own.
        submit(&mut book, 1, 1, Side::Sell, 100, 2);
        submit(&mut book, 2, 2, Side::Sell, 100, 4);

        let events = submit_fok(&mut book, 1, 3, Side::Buy, 100, 4);

        // Precheck sees 4 foreign (excluding the self-owned 2) and passes.
        // The real sweep then cancels the self order via STP on the way
        // through, and fills the full 4 against the foreign order.
        assert_eq!(
            events,
            vec![
                Event::Cancelled {
                    account_id: AccountId(1),
                    order_id: OrderId(1),
                },
                Event::Filled {
                    account_id: AccountId(1),
                    order_id: OrderId(3),
                    side: Side::Buy,
                    price: Price(100),
                    qty: Qty(4),
                    resting_qty: Qty(0),
                    state: FillState::Filled,
                },
                Event::Filled {
                    account_id: AccountId(2),
                    order_id: OrderId(2),
                    side: Side::Sell,
                    price: Price(100),
                    qty: Qty(4),
                    resting_qty: Qty(0),
                    state: FillState::Filled,
                },
                Event::Trade {
                    price: Price(100),
                    qty: Qty(4),
                    taker_side: Side::Buy,
                },
                Event::Accepted {
                    account_id: AccountId(1),
                    order_id: OrderId(3),
                    resting_qty: Qty(0),
                },
            ]
        );
        book.assert_invariants();
        assert_eq!(book.best_ask(), None);
    }

    // -- Market ---------------------------------------------------------

    #[test]
    fn market_partial_fills_and_discards() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 3);
        let events = submit_market(&mut book, 2, 2, Side::Buy, 10);

        assert_eq!(
            events,
            vec![
                Event::Filled {
                    account_id: AccountId(2),
                    order_id: OrderId(2),
                    side: Side::Buy,
                    price: Price(100),
                    qty: Qty(3),
                    resting_qty: Qty(7),
                    state: FillState::PartiallyFilled,
                },
                Event::Filled {
                    account_id: AccountId(1),
                    order_id: OrderId(1),
                    side: Side::Sell,
                    price: Price(100),
                    qty: Qty(3),
                    resting_qty: Qty(0),
                    state: FillState::Filled,
                },
                Event::Trade {
                    price: Price(100),
                    qty: Qty(3),
                    taker_side: Side::Buy,
                },
                Event::Accepted {
                    account_id: AccountId(2),
                    order_id: OrderId(2),
                    resting_qty: Qty(0),
                },
            ]
        );
        book.assert_invariants();
        assert_eq!(book.best_ask(), None);
        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn market_against_empty_book_fills_nothing_and_never_rejects() {
        let mut book = Book::new();
        let events = submit_market(&mut book, 1, 1, Side::Buy, 5);
        assert_eq!(
            events,
            vec![Event::Accepted {
                account_id: AccountId(1),
                order_id: OrderId(1),
                resting_qty: Qty(0),
            }]
        );
        book.assert_invariants();
        assert_eq!(book.best_bid(), None);
    }

    // -- PostOnly ---------------------------------------------------------

    #[test]
    fn postonly_rejects_a_crossing_order() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        let events = submit_postonly(&mut book, 2, 2, Side::Buy, 100, 3);

        assert_eq!(
            events,
            vec![Event::Rejected {
                account_id: AccountId(2),
                order_id: OrderId(2),
                reason: RejectReason::WouldCross,
            }]
        );
        book.assert_invariants();
        assert_eq!(book.best_ask(), Some(Price(100)));
    }

    #[test]
    fn postonly_rests_when_not_crossing() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        let events = submit_postonly(&mut book, 2, 2, Side::Buy, 99, 3);

        assert_eq!(
            events,
            vec![Event::Accepted {
                account_id: AccountId(2),
                order_id: OrderId(2),
                resting_qty: Qty(3),
            }]
        );
        book.assert_invariants();
        assert_eq!(book.best_bid(), Some(Price(99)));
    }

    #[test]
    fn postonly_crossing_only_its_own_resting_order_cancels_and_rests() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        // Same account as the resting order it would cross.
        let events = submit_postonly(&mut book, 1, 2, Side::Buy, 100, 3);

        assert_eq!(
            events,
            vec![
                Event::Cancelled {
                    account_id: AccountId(1),
                    order_id: OrderId(1),
                },
                Event::Accepted {
                    account_id: AccountId(1),
                    order_id: OrderId(2),
                    resting_qty: Qty(3),
                },
            ]
        );
        book.assert_invariants();
        assert_eq!(book.best_ask(), None);
        assert_eq!(book.best_bid(), Some(Price(100)));
    }

    #[test]
    fn postonly_crossing_self_owned_and_foreign_depth_cancels_self_and_still_rejects() {
        let mut book = Book::new();
        // Account 2's own order rests first (head of the level)...
        submit(&mut book, 2, 1, Side::Sell, 100, 2);
        // ...then foreign depth behind it, from a different account.
        submit(&mut book, 1, 2, Side::Sell, 100, 3);
        book.assert_invariants();

        // Account 2's PostOnly buy crosses both: its own order at the head
        // is cancelled for real via STP as the walk reaches it, then the
        // walk hits account 1's foreign order and rejects. The
        // cancellation stands regardless of the reject that follows --
        // the eager-vs-atomic decision recorded in SPEC §2. This is the
        // case the quantity-conservation property test's shrunk failure
        // found.
        let events = submit_postonly(&mut book, 2, 3, Side::Buy, 100, 5);

        assert_eq!(
            events,
            vec![
                Event::Cancelled {
                    account_id: AccountId(2),
                    order_id: OrderId(1),
                },
                Event::Rejected {
                    account_id: AccountId(2),
                    order_id: OrderId(3),
                    reason: RejectReason::WouldCross,
                },
            ]
        );
        book.assert_invariants();

        // Account 2's own order is gone. Account 1's foreign order still
        // rests, untouched. The PostOnly order itself never rests.
        assert_eq!(book.best_ask(), Some(Price(100)));
        let remaining_ask_qty: u64 = book.asks.values().map(|l| l.total_qty.0).sum();
        assert_eq!(remaining_ask_qty, 3);
    }

    // -- Modify -----------------------------------------------------------

    #[test]
    fn modify_decrease_retains_priority() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5); // A, head
        submit(&mut book, 2, 2, Side::Sell, 100, 3); // B, behind A
        book.assert_invariants();

        let events = modify(&mut book, 1, 1, 100, 2);
        assert_eq!(
            events,
            vec![Event::Replaced {
                account_id: AccountId(1),
                order_id: OrderId(1),
                new_qty: Qty(2),
                priority_retained: true,
            }]
        );
        book.assert_invariants();

        // A still has priority: an aggressor for exactly A's new size
        // must fill only A, never touching B.
        let fills = submit(&mut book, 9, 99, Side::Buy, 100, 2);
        assert!(
            fills.iter().any(
                |e| matches!(e, Event::Filled { account_id, .. } if *account_id == AccountId(1))
            )
        );
        assert!(
            !fills.iter().any(
                |e| matches!(e, Event::Filled { account_id, .. } if *account_id == AccountId(2))
            )
        );
    }

    #[test]
    fn modify_increase_loses_priority() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 2); // A, head
        submit(&mut book, 2, 2, Side::Sell, 100, 3); // B, behind A
        book.assert_invariants();

        let events = modify(&mut book, 1, 1, 100, 5);
        assert_eq!(
            events,
            vec![Event::Replaced {
                account_id: AccountId(1),
                order_id: OrderId(1),
                new_qty: Qty(5),
                priority_retained: false,
            }]
        );
        book.assert_invariants();

        // A moved to the back: an aggressor for exactly B's size must
        // fill only B.
        let fills = submit(&mut book, 9, 99, Side::Buy, 100, 3);
        assert!(
            fills.iter().any(
                |e| matches!(e, Event::Filled { account_id, .. } if *account_id == AccountId(2))
            )
        );
        assert!(
            !fills.iter().any(
                |e| matches!(e, Event::Filled { account_id, .. } if *account_id == AccountId(1))
            )
        );
    }

    #[test]
    fn modify_price_change_without_crossing_moves_to_new_level() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Buy, 90, 5);
        book.assert_invariants();

        let events = modify(&mut book, 1, 1, 95, 5);
        assert_eq!(
            events,
            vec![Event::Replaced {
                account_id: AccountId(1),
                order_id: OrderId(1),
                new_qty: Qty(5),
                priority_retained: false,
            }]
        );
        book.assert_invariants();
        assert_eq!(book.best_bid(), Some(Price(95)));
    }

    #[test]
    fn modify_price_change_crosses_emits_replaced_then_filled() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Buy, 90, 5); // non-crossing at 90
        submit(&mut book, 2, 2, Side::Sell, 95, 3); // foreign ask, untouched by the 90 bid
        book.assert_invariants();

        // Repricing the bid to 100 crosses the 95 ask. Exact event
        // sequence, not just presence: Replaced first, then the Filled
        // pair the crossing produces (SPEC §2).
        let events = modify(&mut book, 1, 1, 100, 5);
        assert_eq!(
            events,
            vec![
                Event::Replaced {
                    account_id: AccountId(1),
                    order_id: OrderId(1),
                    new_qty: Qty(5),
                    priority_retained: false,
                },
                Event::Filled {
                    account_id: AccountId(1),
                    order_id: OrderId(1),
                    side: Side::Buy,
                    price: Price(95),
                    qty: Qty(3),
                    resting_qty: Qty(2),
                    state: FillState::PartiallyFilled,
                },
                Event::Filled {
                    account_id: AccountId(2),
                    order_id: OrderId(2),
                    side: Side::Sell,
                    price: Price(95),
                    qty: Qty(3),
                    resting_qty: Qty(0),
                    state: FillState::Filled,
                },
                Event::Trade {
                    price: Price(95),
                    qty: Qty(3),
                    taker_side: Side::Buy,
                },
            ]
        );
        book.assert_invariants();
        // Remainder (2) rests at the new price.
        assert_eq!(book.best_bid(), Some(Price(100)));
        assert_eq!(book.best_ask(), None);
    }

    #[test]
    fn modify_rejects_zero_quantity() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Buy, 100, 5);
        let events = modify(&mut book, 1, 1, 100, 0);
        assert_eq!(
            events,
            vec![Event::Rejected {
                account_id: AccountId(1),
                order_id: OrderId(1),
                reason: RejectReason::ZeroQuantity,
            }]
        );
        book.assert_invariants();
        assert_eq!(book.best_bid(), Some(Price(100)));
    }

    #[test]
    fn modify_ownership_rejected_identically_to_unknown_id() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5);

        let wrong_account = modify(&mut book, 2, 1, 100, 3);
        let unknown_id = modify(&mut book, 2, 999, 100, 3);

        assert_eq!(
            wrong_account,
            vec![Event::Rejected {
                account_id: AccountId(2),
                order_id: OrderId(1),
                reason: RejectReason::UnknownOrderId,
            }]
        );
        assert_eq!(
            unknown_id,
            vec![Event::Rejected {
                account_id: AccountId(2),
                order_id: OrderId(999),
                reason: RejectReason::UnknownOrderId,
            }]
        );
        book.assert_invariants();
        assert_eq!(book.best_ask(), Some(Price(100)));
    }

    // -- Self-trade prevention --------------------------------------------

    #[test]
    fn stp_cancels_resting_side() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        // Same account crosses its own resting order.
        let events = submit(&mut book, 1, 2, Side::Buy, 100, 3);

        assert_eq!(
            events,
            vec![
                Event::Cancelled {
                    account_id: AccountId(1),
                    order_id: OrderId(1),
                },
                Event::Accepted {
                    account_id: AccountId(1),
                    order_id: OrderId(2),
                    resting_qty: Qty(3),
                },
            ]
        );
        book.assert_invariants();
        // The aggressor's quantity was not consumed by the STP cancel --
        // all 3 lots rest, not fewer.
        assert_eq!(book.best_bid(), Some(Price(100)));
        assert_eq!(book.best_ask(), None);
    }

    #[test]
    fn stp_continues_past_a_cancelled_maker() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 3); // self, head
        submit(&mut book, 2, 2, Side::Sell, 100, 4); // foreign, behind
        book.assert_invariants();

        let events = submit(&mut book, 1, 3, Side::Buy, 100, 5);

        assert_eq!(
            events,
            vec![
                Event::Cancelled {
                    account_id: AccountId(1),
                    order_id: OrderId(1),
                },
                Event::Filled {
                    account_id: AccountId(1),
                    order_id: OrderId(3),
                    side: Side::Buy,
                    price: Price(100),
                    qty: Qty(4),
                    resting_qty: Qty(1),
                    state: FillState::PartiallyFilled,
                },
                Event::Filled {
                    account_id: AccountId(2),
                    order_id: OrderId(2),
                    side: Side::Sell,
                    price: Price(100),
                    qty: Qty(4),
                    resting_qty: Qty(0),
                    state: FillState::Filled,
                },
                Event::Trade {
                    price: Price(100),
                    qty: Qty(4),
                    taker_side: Side::Buy,
                },
                Event::Accepted {
                    account_id: AccountId(1),
                    order_id: OrderId(3),
                    resting_qty: Qty(1),
                },
            ]
        );
        book.assert_invariants();
        assert_eq!(book.best_ask(), None);
        assert_eq!(book.best_bid(), Some(Price(100)));
    }

    #[test]
    fn stp_across_consecutive_same_account_levels() {
        let mut book = Book::new();
        // Two consecutive price levels, both entirely owned by the
        // aggressor's own account, no foreign depth anywhere.
        submit(&mut book, 1, 1, Side::Sell, 100, 2);
        submit(&mut book, 1, 2, Side::Sell, 101, 3);
        book.assert_invariants();

        let events = submit(&mut book, 1, 3, Side::Buy, 101, 10);

        assert_eq!(
            events,
            vec![
                Event::Cancelled {
                    account_id: AccountId(1),
                    order_id: OrderId(1),
                },
                Event::Cancelled {
                    account_id: AccountId(1),
                    order_id: OrderId(2),
                },
                Event::Accepted {
                    account_id: AccountId(1),
                    order_id: OrderId(3),
                    resting_qty: Qty(10),
                },
            ]
        );
        book.assert_invariants();
        assert_eq!(book.best_ask(), None);
        assert_eq!(book.best_bid(), Some(Price(101)));
    }

    // -- Property test: quantity conservation ------------------------------
    //
    // `submitted == 2*filled + resting + cancelled + discarded` (SPEC §6).
    // A single match of size k consumes k units from *both* the taker's and
    // the maker's originally-submitted totals, hence `2*filled`.
    //
    // The model below is a shadow ledger built purely from each
    // operation's emitted `Event`s (never by re-deriving fills
    // independently), so a bug in `sweep`/`rest`/`unlink` that still
    // happens to emit a self-consistent-looking event stream is the only
    // way this could pass when it shouldn't -- which is exactly the same
    // class of bug `assert_invariants()` (also checked after every
    // operation here) is positioned to catch from the other direction.

    #[derive(Debug, Clone)]
    enum Op {
        Gtc {
            account: u64,
            order: u64,
            side: Side,
            price: u64,
            qty: u64,
        },
        Ioc {
            account: u64,
            order: u64,
            side: Side,
            price: u64,
            qty: u64,
        },
        Fok {
            account: u64,
            order: u64,
            side: Side,
            price: u64,
            qty: u64,
        },
        PostOnly {
            account: u64,
            order: u64,
            side: Side,
            price: u64,
            qty: u64,
        },
        Market {
            account: u64,
            order: u64,
            side: Side,
            qty: u64,
        },
        Cancel {
            account: u64,
            order: u64,
        },
        Modify {
            account: u64,
            order: u64,
            price: u64,
            qty: u64,
        },
        MassCancel {
            account: u64,
        },
    }

    fn side_strategy() -> impl Strategy<Value = Side> {
        prop_oneof![Just(Side::Buy), Just(Side::Sell)]
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        let account = 1u64..=3;
        let order = 1u64..=5;
        let price = 95u64..=105;
        let qty = 1u64..=5;
        prop_oneof![
            (
                account.clone(),
                order.clone(),
                side_strategy(),
                price.clone(),
                qty.clone()
            )
                .prop_map(|(account, order, side, price, qty)| Op::Gtc {
                    account,
                    order,
                    side,
                    price,
                    qty
                }),
            (
                account.clone(),
                order.clone(),
                side_strategy(),
                price.clone(),
                qty.clone()
            )
                .prop_map(|(account, order, side, price, qty)| Op::Ioc {
                    account,
                    order,
                    side,
                    price,
                    qty
                }),
            (
                account.clone(),
                order.clone(),
                side_strategy(),
                price.clone(),
                qty.clone()
            )
                .prop_map(|(account, order, side, price, qty)| Op::Fok {
                    account,
                    order,
                    side,
                    price,
                    qty
                }),
            (
                account.clone(),
                order.clone(),
                side_strategy(),
                price.clone(),
                qty.clone()
            )
                .prop_map(|(account, order, side, price, qty)| Op::PostOnly {
                    account,
                    order,
                    side,
                    price,
                    qty
                }),
            (account.clone(), order.clone(), side_strategy(), qty.clone()).prop_map(
                |(account, order, side, qty)| Op::Market {
                    account,
                    order,
                    side,
                    qty
                }
            ),
            (account.clone(), order.clone())
                .prop_map(|(account, order)| Op::Cancel { account, order }),
            (account.clone(), order.clone(), price.clone(), qty.clone()).prop_map(
                |(account, order, price, qty)| Op::Modify {
                    account,
                    order,
                    price,
                    qty
                }
            ),
            account.prop_map(|account| Op::MassCancel { account }),
        ]
    }

    /// Shadow ledger: what the conservation equation's four running totals
    /// should be, plus a per-order shadow of currently-resting quantity
    /// (cross-checked against the book's actual resting total every step).
    #[derive(Default)]
    struct ConservationModel {
        submitted: u64,
        filled: u64,
        cancelled: u64,
        discarded: u64,
        resting: HashMap<(AccountId, OrderId), u64>,
    }

    impl ConservationModel {
        /// Every `Cancelled` in `events` (explicit cancel, mass-cancel, or
        /// an STP cancellation mid-sweep) removes that order from the
        /// shadow and credits its quantity to `cancelled`.
        fn consume_cancelled(&mut self, events: &[Event]) {
            for event in events {
                if let Event::Cancelled {
                    account_id,
                    order_id,
                } = event
                {
                    let id = (*account_id, *order_id);
                    let qty = self
                        .resting
                        .remove(&id)
                        .expect("cancelled order must have been resting in the model");
                    self.cancelled += qty;
                }
            }
        }

        /// Every `Filled` in `events` either belongs to `self_id` (the
        /// order this operation submitted or modified -- tallied into the
        /// returned total, not into `filled`, to avoid double-counting
        /// the two `Filled`s a single match produces) or to a maker
        /// (shrinks that maker's shadow resting qty, credits `filled`
        /// once). Returns how much of `self_id`'s own quantity filled.
        fn consume_filled(&mut self, self_id: (AccountId, OrderId), events: &[Event]) -> u64 {
            let mut self_filled = 0u64;
            for event in events {
                if let Event::Filled {
                    account_id,
                    order_id,
                    qty,
                    ..
                } = event
                {
                    let id = (*account_id, *order_id);
                    if id == self_id {
                        self_filled += qty.0;
                    } else {
                        let entry = self
                            .resting
                            .get_mut(&id)
                            .expect("maker must have been resting in the model");
                        *entry -= qty.0;
                        if *entry == 0 {
                            self.resting.remove(&id);
                        }
                        self.filled += qty.0;
                    }
                }
            }
            self_filled
        }

        /// A rejected operation (single `Rejected` event, per every reject
        /// path in `Book`) had zero effect on the book -- nothing to
        /// record.
        fn is_rejected(events: &[Event]) -> bool {
            events.iter().any(|e| matches!(e, Event::Rejected { .. }))
        }

        fn record_submit(
            &mut self,
            self_id: (AccountId, OrderId),
            qty: u64,
            rests_if_unfilled: bool,
            events: &[Event],
        ) {
            // Process STP cancellations regardless of the final outcome:
            // PostOnly's WouldCross reject can be preceded by a real
            // same-account cancellation the sweep already performed before
            // it found foreign depth and gave up (SPEC §2's eager-vs-atomic
            // decision — cancellations already performed are not
            // reversed). Every other reject path checks before mutating
            // anything, so this is a no-op for them.
            self.consume_cancelled(events);

            if Self::is_rejected(events) {
                return;
            }
            self.submitted += qty;
            let self_filled = self.consume_filled(self_id, events);
            let resting_after = qty - self_filled;
            if resting_after > 0 {
                if rests_if_unfilled {
                    self.resting.insert(self_id, resting_after);
                } else {
                    self.discarded += resting_after;
                }
            }
        }

        fn record_modify(&mut self, self_id: (AccountId, OrderId), events: &[Event]) {
            if Self::is_rejected(events) {
                return;
            }
            let new_qty = match &events[0] {
                Event::Replaced { new_qty, .. } => new_qty.0,
                other => panic!("modify's first event must be Replaced, got {other:?}"),
            };
            let old_qty = *self
                .resting
                .get(&self_id)
                .expect("modified order must have been resting in the model");

            if new_qty >= old_qty {
                self.submitted += new_qty - old_qty;
            } else {
                self.cancelled += old_qty - new_qty;
            }

            // Everything after Replaced is the re-entered sweep, if any
            // (empty for a retained-priority in-place update).
            self.consume_cancelled(&events[1..]);
            let self_filled = self.consume_filled(self_id, &events[1..]);
            let resting_after = new_qty - self_filled;
            // Modify never discards -- any remainder always rests
            // (SPEC §2) -- so unlike record_submit there is no
            // rests_if_unfilled branch.
            if resting_after > 0 {
                self.resting.insert(self_id, resting_after);
            } else {
                self.resting.remove(&self_id);
            }
        }

        fn record_cancel_or_mass_cancel(&mut self, events: &[Event]) {
            if Self::is_rejected(events) {
                return;
            }
            self.consume_cancelled(events);
        }

        fn assert_conserved(&self, book: &Book) {
            let resting_in_book: u64 = book.bids.values().map(|l| l.total_qty.0).sum::<u64>()
                + book.asks.values().map(|l| l.total_qty.0).sum::<u64>();
            let resting_in_model: u64 = self.resting.values().sum();
            assert_eq!(
                resting_in_book, resting_in_model,
                "model's shadow resting total drifted from the book's actual resting total"
            );
            assert_eq!(
                self.submitted,
                2 * self.filled + resting_in_book + self.cancelled + self.discarded,
                "quantity conservation violated: submitted={}, filled={}, resting={}, cancelled={}, discarded={}",
                self.submitted,
                self.filled,
                resting_in_book,
                self.cancelled,
                self.discarded
            );
        }
    }

    fn apply_op(book: &mut Book, model: &mut ConservationModel, op: Op) {
        match op {
            Op::Gtc {
                account,
                order,
                side,
                price,
                qty,
            } => {
                let events = submit(book, account, order, side, price, qty);
                model.record_submit((AccountId(account), OrderId(order)), qty, true, &events);
            }
            Op::Ioc {
                account,
                order,
                side,
                price,
                qty,
            } => {
                let events = submit_ioc(book, account, order, side, price, qty);
                model.record_submit((AccountId(account), OrderId(order)), qty, false, &events);
            }
            Op::Fok {
                account,
                order,
                side,
                price,
                qty,
            } => {
                let events = submit_fok(book, account, order, side, price, qty);
                model.record_submit((AccountId(account), OrderId(order)), qty, false, &events);
            }
            Op::PostOnly {
                account,
                order,
                side,
                price,
                qty,
            } => {
                let events = submit_postonly(book, account, order, side, price, qty);
                model.record_submit((AccountId(account), OrderId(order)), qty, true, &events);
            }
            Op::Market {
                account,
                order,
                side,
                qty,
            } => {
                let events = submit_market(book, account, order, side, qty);
                model.record_submit((AccountId(account), OrderId(order)), qty, false, &events);
            }
            Op::Cancel { account, order } => {
                let events = cancel(book, account, order);
                model.record_cancel_or_mass_cancel(&events);
            }
            Op::Modify {
                account,
                order,
                price,
                qty,
            } => {
                let events = modify(book, account, order, price, qty);
                model.record_modify((AccountId(account), OrderId(order)), &events);
            }
            Op::MassCancel { account } => {
                let events = mass_cancel(book, account);
                model.record_cancel_or_mass_cancel(&events);
            }
        }
    }

    proptest! {
        #[test]
        fn quantity_conservation(ops in proptest::collection::vec(op_strategy(), 1..40)) {
            let mut book = Book::new();
            let mut model = ConservationModel::default();

            for op in ops {
                apply_op(&mut book, &mut model, op);
                book.assert_invariants();
                model.assert_conserved(&book);
            }
        }
    }

    // -- Accessors risk (stage 4) reads --------------------------------

    #[test]
    fn last_trade_is_none_until_a_fill_happens() {
        let mut book = Book::new();
        assert_eq!(book.last_trade(), None);
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        assert_eq!(
            book.last_trade(),
            None,
            "resting with no crossing is not a trade"
        );
        submit(&mut book, 2, 2, Side::Buy, 100, 5);
        assert_eq!(book.last_trade(), Some(Price(100)));
    }

    #[test]
    fn last_trade_reflects_the_most_recent_fill() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        submit(&mut book, 1, 2, Side::Sell, 105, 5);
        submit(&mut book, 2, 3, Side::Buy, 105, 10);
        // Sweeps 100 first (price priority), then 105 -- last_trade
        // reflects the LAST match, not the first.
        assert_eq!(book.last_trade(), Some(Price(105)));
    }

    #[test]
    fn trade_price_matches_last_trade_for_every_fill_in_a_sweep() {
        // Two levels, distinct prices, one aggressor that sweeps both --
        // each Trade's price must equal book.last_trade() as observed
        // immediately after that specific fill, proving both come from
        // the same read rather than two independent computations that
        // could drift apart.
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 3);
        submit(&mut book, 1, 2, Side::Sell, 105, 3);
        let events = submit(&mut book, 2, 3, Side::Buy, 105, 6);

        let trades: Vec<&Event> = events
            .iter()
            .filter(|e| matches!(e, Event::Trade { .. }))
            .collect();
        assert_eq!(
            trades,
            vec![
                &Event::Trade {
                    price: Price(100),
                    qty: Qty(3),
                    taker_side: Side::Buy,
                },
                &Event::Trade {
                    price: Price(105),
                    qty: Qty(3),
                    taker_side: Side::Buy,
                },
            ]
        );
        // The last Trade's price is exactly what last_trade() reports
        // once the whole sweep has settled.
        assert_eq!(book.last_trade(), Some(Price(105)));
    }

    #[test]
    fn stp_cancellation_produces_no_trade() {
        // A self-trade-prevented cancellation is not an execution -- it
        // must not print a Trade, and must not move last_trade.
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        let events = submit(&mut book, 1, 2, Side::Buy, 100, 3);
        assert!(!events.iter().any(|e| matches!(e, Event::Trade { .. })));
        assert_eq!(book.last_trade(), None);
    }

    #[test]
    fn account_open_order_count_and_notional_are_zero_for_an_unseen_account() {
        let book = Book::new();
        assert_eq!(book.account_open_order_count(AccountId(1)), 0);
        assert_eq!(book.account_notional(AccountId(1)), 0);
    }

    #[test]
    fn account_open_order_count_and_notional_reflect_resting_orders() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        submit(&mut book, 1, 2, Side::Sell, 200, 3);
        assert_eq!(book.account_open_order_count(AccountId(1)), 2);
        assert_eq!(book.account_notional(AccountId(1)), 100 * 5 + 200 * 3);
    }

    #[test]
    fn resting_order_snapshot_reflects_current_price_and_qty() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        assert_eq!(
            book.resting_order_snapshot(AccountId(1), OrderId(1)),
            Some((Price(100), Qty(5)))
        );

        modify(&mut book, 1, 1, 100, 2); // decrease, retains priority
        assert_eq!(
            book.resting_order_snapshot(AccountId(1), OrderId(1)),
            Some((Price(100), Qty(2)))
        );
    }

    #[test]
    fn resting_order_snapshot_is_none_for_unknown_or_wrong_account() {
        let mut book = Book::new();
        submit(&mut book, 1, 1, Side::Sell, 100, 5);
        assert_eq!(
            book.resting_order_snapshot(AccountId(1), OrderId(999)),
            None
        );
        assert_eq!(book.resting_order_snapshot(AccountId(2), OrderId(1)), None);
    }
}
