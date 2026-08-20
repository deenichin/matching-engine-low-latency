//! Allocation proof (SPEC §6, §9), behind `count-allocations`. Both tests
//! route commands through `risk::process_command`, not `Engine::apply`
//! directly -- CLAUDE.md's hot-path rule explicitly includes the risk
//! check in what must not allocate, and `process_command` is the exact
//! shared function the live matching thread and replay (stage 6) already
//! call. Measuring `Engine::apply` alone would silently skip proving the
//! risk layer is allocation-free.
//!
//! `hot_path_allocates_nothing` runs its exact measured command shape
//! *twice* against the same accounts and the same price levels -- fresh
//! order ids the second time, so nothing rejects as a duplicate -- and
//! only the second run is measured. Every one-time cost a warm engine can
//! still legitimately pay on a *new* account or a *new* price level
//! (`AccountEntry::slots`'s first reservation, a `HashMap` bucket-array
//! resize the first time enough distinct accounts exist, `Book`'s
//! `mass_cancel_scratch` growing to cover its first mass-cancel, a
//! `BTreeMap` node split on a level nobody has used before) happens during
//! the first, unmeasured run; the second run touches nothing that isn't
//! already warm. This was arrived at empirically, not by inspection: an
//! earlier version pre-warmed accounts individually and still measured a
//! stray allocation on `Modify increase` traced (by isolating every
//! command with its own `measure` call, then toggling pieces of warm-up
//! on and off) to a `HashMap` resize on `Book::accounts` triggered by
//! which specific account first pushed the map over a growth threshold --
//! a real, structural warm-up gap, not a bug in the fix to
//! `Book::mass_cancel` that prompted it. Running the whole shape twice is
//! the general fix: it doesn't require knowing in advance which internal
//! structure will need its first touch.

use core::Engine;
use core::event::{Command, Event};
use core::types::{AccountId, OrderId, OrderKind, Price, Qty, Side, Tif};
use risk::{RiskConfig, RiskState};
use std::collections::VecDeque;

// `allocation-counter` registers its own `#[global_allocator]` internally
// (SPEC §9: "its unsafe impl GlobalAlloc lives in the dependency, not in
// this source") -- declaring one here too would conflict with it.

fn discard(_: Event) {}

fn new_order(
    account: u64,
    order: u64,
    side: Side,
    price: u64,
    qty: u64,
    kind: OrderKind,
    tif: Tif,
) -> Command {
    Command::NewOrder {
        account_id: AccountId(account),
        order_id: OrderId(order),
        side,
        price: Price(price),
        qty: Qty(qty),
        kind,
        tif,
        client_ts: 0,
    }
}

fn apply(engine: &mut Engine, risk_state: &mut RiskState, cmd: Command) {
    risk::process_command(engine, risk_state, cmd, &mut discard);
}

// -- hot_path_allocates_nothing ------------------------------------------

/// Every account and price level the sequence below touches:
/// - 1 (Sell@100, background depth) and 2 (Buy@90, background depth).
/// - 3: GTC no-cross, then a Market taker.
/// - 4: IOC crossing taker. 5: FOK crossing taker. 6: PostOnly, resting
///   at 80 (a price no other order in this sequence uses).
/// - 7: rests at 95 -- a level dedicated to it alone, within the price
///   band around any `last_trade` this sequence can produce (90-100, so
///   +-10% always covers 95) -- then crosses its own order there, so STP
///   actually triggers (crossing at a *shared* level would match whoever
///   rested first instead, per price-time priority, never reaching this
///   order at all).
/// - 8: plain cancel target. 9: modify-decrease target. 10:
///   modify-increase target. 11: mass-cancel target, two resting orders.
///
/// `order_id_base` lets this run twice against the same accounts without
/// a `DuplicateOrderId` reject the second time.
fn run_sequence(engine: &mut Engine, risk_state: &mut RiskState, order_id_base: u64) {
    // Fresh targets for this pass.
    apply(
        engine,
        risk_state,
        new_order(
            7,
            order_id_base,
            Side::Sell,
            95,
            5,
            OrderKind::Limit,
            Tif::Gtc,
        ),
    );
    apply(
        engine,
        risk_state,
        new_order(
            8,
            order_id_base,
            Side::Sell,
            100,
            5,
            OrderKind::Limit,
            Tif::Gtc,
        ),
    );
    apply(
        engine,
        risk_state,
        new_order(
            9,
            order_id_base,
            Side::Sell,
            100,
            5,
            OrderKind::Limit,
            Tif::Gtc,
        ),
    );
    apply(
        engine,
        risk_state,
        new_order(
            10,
            order_id_base,
            Side::Sell,
            100,
            5,
            OrderKind::Limit,
            Tif::Gtc,
        ),
    );
    apply(
        engine,
        risk_state,
        new_order(
            11,
            order_id_base,
            Side::Sell,
            100,
            5,
            OrderKind::Limit,
            Tif::Gtc,
        ),
    );
    apply(
        engine,
        risk_state,
        new_order(
            11,
            order_id_base + 1,
            Side::Buy,
            90,
            5,
            OrderKind::Limit,
            Tif::Gtc,
        ),
    );

    // GTC: rests without crossing, appends to the existing ask level at
    // 100.
    apply(
        engine,
        risk_state,
        new_order(
            3,
            order_id_base,
            Side::Sell,
            100,
            1,
            OrderKind::Limit,
            Tif::Gtc,
        ),
    );

    // IOC: crosses the existing bid level at 90 partially, discards the
    // remainder.
    apply(
        engine,
        risk_state,
        new_order(
            4,
            order_id_base,
            Side::Sell,
            90,
            10,
            OrderKind::Limit,
            Tif::Ioc,
        ),
    );

    // FOK: fully fillable against the existing ask level at 100.
    apply(
        engine,
        risk_state,
        new_order(
            5,
            order_id_base,
            Side::Buy,
            100,
            1,
            OrderKind::Limit,
            Tif::Fok,
        ),
    );

    // PostOnly: rests without crossing.
    apply(
        engine,
        risk_state,
        new_order(
            6,
            order_id_base,
            Side::Buy,
            80,
            1,
            OrderKind::Limit,
            Tif::PostOnly,
        ),
    );

    // Market: sweeps existing bid depth (account 2's level at 90).
    apply(
        engine,
        risk_state,
        new_order(
            3,
            order_id_base + 1,
            Side::Sell,
            0,
            1,
            OrderKind::Market,
            Tif::Ioc,
        ),
    );

    // STP-triggering cross: account 7 crosses its own resting order at
    // 95, the only thing resting there.
    apply(
        engine,
        risk_state,
        new_order(
            7,
            order_id_base + 1,
            Side::Buy,
            95,
            5,
            OrderKind::Limit,
            Tif::Gtc,
        ),
    );

    // Plain cancel of an existing resting order.
    apply(
        engine,
        risk_state,
        Command::CancelOrder {
            account_id: AccountId(8),
            order_id: OrderId(order_id_base),
        },
    );

    // Modify, decrease (retains priority, in place).
    apply(
        engine,
        risk_state,
        Command::CancelReplace {
            account_id: AccountId(9),
            order_id: OrderId(order_id_base),
            new_price: Price(100),
            new_qty: Qty(2),
        },
    );

    // Modify, increase (loses priority, re-enters at the back of the
    // same, already-existing level).
    apply(
        engine,
        risk_state,
        Command::CancelReplace {
            account_id: AccountId(10),
            order_id: OrderId(order_id_base),
            new_price: Price(100),
            new_qty: Qty(10),
        },
    );

    // Mass-cancel across an account's several resting orders.
    apply(
        engine,
        risk_state,
        Command::MassCancel {
            account_id: AccountId(11),
        },
    );
}

fn setup() -> (Engine, RiskState) {
    let mut engine = Engine::new();
    let mut risk_state = RiskState::new(RiskConfig::default());

    apply(
        &mut engine,
        &mut risk_state,
        new_order(1, 1, Side::Sell, 100, 500, OrderKind::Limit, Tif::Gtc),
    );
    apply(
        &mut engine,
        &mut risk_state,
        new_order(2, 1, Side::Buy, 90, 500, OrderKind::Limit, Tif::Gtc),
    );

    // Unmeasured dry run: warms every account, every price level, and
    // every internal structure (`Book::accounts`'s `HashMap`,
    // `mass_cancel_scratch`, each `AccountEntry::slots`, the B-tree nodes
    // for 80/90/95/100) the measured run below will touch, so the
    // measured run pays none of their first-touch costs.
    run_sequence(&mut engine, &mut risk_state, 1_000);

    (engine, risk_state)
}

#[test]
fn hot_path_allocates_nothing() {
    let (mut engine, mut risk_state) = setup();

    let info = allocation_counter::measure(|| {
        run_sequence(&mut engine, &mut risk_state, 2_000);
    });

    assert_eq!(
        info.count_total, 0,
        "hot path against existing price levels and existing accounts must allocate nothing; got {info:?}"
    );
}

// -- level_churn_allocation_is_bounded ------------------------------------

const CHURN_ACCOUNTS: u64 = 20;
const CHURN_BAND_WIDTH: u64 = 100;
const CHURN_OPERATIONS: u64 = 10_000;
const CHURN_BASE_PRICE: u64 = 5_000;

/// Records -- doesn't assert zero -- allocations per 1,000 operations
/// across a realistic price band that keeps creating and destroying
/// levels: `BTreeMap` only allocates on a node split (not on every key
/// insert) and only deallocates on a merge, so this is expected to be
/// small but non-zero, and is reported as a rate rather than a per-level
/// figure -- a per-level figure would overstate it, since most level
/// creations don't split a node.
#[test]
fn level_churn_allocation_is_bounded() {
    let mut engine = Engine::new();
    let mut risk_state = RiskState::new(RiskConfig::default());

    // Pre-warm every account this test uses -- account creation isn't
    // what's being measured here either; only level churn is.
    for account in 1..=CHURN_ACCOUNTS {
        apply(
            &mut engine,
            &mut risk_state,
            new_order(account, 1, Side::Buy, 1, 1, OrderKind::Limit, Tif::Gtc),
        );
        apply(
            &mut engine,
            &mut risk_state,
            Command::CancelOrder {
                account_id: AccountId(account),
                order_id: OrderId(1),
            },
        );
    }

    // A sliding window of CHURN_BAND_WIDTH distinct prices: each step
    // submits a new order at the next price in the band, and once the
    // window is full, cancels the order submitted CHURN_BAND_WIDTH steps
    // ago -- keeping roughly CHURN_BAND_WIDTH price levels active at any
    // time, continuously creating and destroying levels at the edges.
    let mut window: VecDeque<(AccountId, OrderId, u64)> =
        VecDeque::with_capacity(CHURN_BAND_WIDTH as usize);
    let mut next_order_id = vec![1u64; (CHURN_ACCOUNTS + 1) as usize];
    let mut operations = 0u64;

    let info = allocation_counter::measure(|| {
        for i in 0..CHURN_OPERATIONS {
            let account = (i % CHURN_ACCOUNTS) + 1;
            let order_id = next_order_id[account as usize];
            next_order_id[account as usize] += 1;
            let price = CHURN_BASE_PRICE + (i % CHURN_BAND_WIDTH);

            apply(
                &mut engine,
                &mut risk_state,
                new_order(
                    account,
                    order_id,
                    Side::Buy,
                    price,
                    1,
                    OrderKind::Limit,
                    Tif::Gtc,
                ),
            );
            operations += 1;
            window.push_back((AccountId(account), OrderId(order_id), price));

            if window.len() as u64 > CHURN_BAND_WIDTH {
                let (old_account, old_order, _price) =
                    window.pop_front().expect("just checked len > 0");
                apply(
                    &mut engine,
                    &mut risk_state,
                    Command::CancelOrder {
                        account_id: old_account,
                        order_id: old_order,
                    },
                );
                operations += 1;
            }
        }
    });

    let per_1000 = info.count_total as f64 / operations as f64 * 1000.0;
    println!(
        "level_churn_allocation_is_bounded: {} allocations over {operations} operations \
         ({per_1000:.4} per 1,000 operations)",
        info.count_total
    );

    // Not a correctness assertion -- BENCH.md is where this rate gets
    // interpreted. A sanity ceiling only, to catch a gross regression
    // (e.g. an accidental per-operation allocation) without pretending to
    // assert the exact rate.
    assert!(
        per_1000 < 500.0,
        "level churn allocation rate looks pathological: {per_1000:.4} per 1,000 operations ({} total over {operations} operations)",
        info.count_total
    );
}
