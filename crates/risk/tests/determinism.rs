//! Stage 6 determinism property test (SPEC §7, PLAN.md stage 6 items 5, 7,
//! 8): the same `Command` sequence run twice through `risk::process_command`
//! — the exact function replay uses — against two fresh `Engine`/`RiskState`
//! pairs must produce byte-for-byte identical output, including
//! `last_trade`, which has no direct event-stream representation and so
//! gets its own explicit comparison rather than relying on the event
//! stream alone to prove it.
//!
//! Every generated sequence deliberately contains a guaranteed trade
//! followed unconditionally by a `Market` order and a price-anchored
//! `Limit` order — not merely "possibly" reachable through the random op
//! set, which is exactly the gap `quantity_conservation` had with `Market`
//! before `RiskState` existed (stage 1, pre-stage-4): being in the op set
//! doesn't prove the branch was ever exercised.

use core::Engine;
use core::event::{Command, Event};
use core::types::{AccountId, EngineSeq, OrderId, OrderKind, Price, Qty, Side, Tif};
use proptest::prelude::*;
use risk::{RiskConfig, RiskState};

#[derive(Debug, Clone)]
enum Op {
    NewOrder {
        account: u64,
        order: u64,
        side: Side,
        price: u64,
        qty: u64,
        tif: Tif,
    },
    Cancel {
        account: u64,
        order: u64,
    },
    CancelReplace {
        account: u64,
        order: u64,
        price: u64,
        qty: u64,
    },
    MassCancel {
        account: u64,
    },
    KillSwitch {
        engaged: bool,
    },
}

fn side_strategy() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Buy), Just(Side::Sell)]
}

fn tif_strategy() -> impl Strategy<Value = Tif> {
    prop_oneof![
        Just(Tif::Gtc),
        Just(Tif::Ioc),
        Just(Tif::Fok),
        Just(Tif::PostOnly)
    ]
}

/// Kept well clear of the guaranteed-trade price (500, below) so nothing
/// generated here can accidentally cross it.
fn op_strategy() -> impl Strategy<Value = Op> {
    let account = 1u64..=5;
    let order = 1u64..=5;
    let price = 95u64..=105;
    let qty = 1u64..=5;
    prop_oneof![
        (
            account.clone(),
            order.clone(),
            side_strategy(),
            price.clone(),
            qty.clone(),
            tif_strategy()
        )
            .prop_map(|(account, order, side, price, qty, tif)| Op::NewOrder {
                account,
                order,
                side,
                price,
                qty,
                tif
            }),
        (account.clone(), order.clone()).prop_map(|(account, order)| Op::Cancel { account, order }),
        (account.clone(), order.clone(), price.clone(), qty.clone()).prop_map(
            |(account, order, price, qty)| Op::CancelReplace {
                account,
                order,
                price,
                qty
            }
        ),
        account
            .clone()
            .prop_map(|account| Op::MassCancel { account }),
        any::<bool>().prop_map(|engaged| Op::KillSwitch { engaged }),
    ]
}

fn new_order_command(
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

fn op_to_command(op: Op) -> Command {
    match op {
        Op::NewOrder {
            account,
            order,
            side,
            price,
            qty,
            tif,
        } => new_order_command(account, order, side, price, qty, OrderKind::Limit, tif),
        Op::Cancel { account, order } => Command::CancelOrder {
            account_id: AccountId(account),
            order_id: OrderId(order),
        },
        Op::CancelReplace {
            account,
            order,
            price,
            qty,
        } => Command::CancelReplace {
            account_id: AccountId(account),
            order_id: OrderId(order),
            new_price: Price(price),
            new_qty: Qty(qty),
        },
        Op::MassCancel { account } => Command::MassCancel {
            account_id: AccountId(account),
        },
        Op::KillSwitch { engaged } => Command::KillSwitch { engaged },
    }
}

/// The price the guaranteed trade below executes at -- far outside
/// `op_strategy`'s 95..=105 range, so nothing randomly generated can ever
/// cross it by accident and the guaranteed section's outcome doesn't
/// depend on what the random prefix left in the book.
const GUARANTEED_TRADE_PRICE: u64 = 500;

/// Runs one full sequence through the exact function replay uses, against
/// a fresh `Engine`/`RiskState` pair. Also collects the `EngineSeq` each
/// event was stamped with -- `engine_seq` is new replayed state (SPEC
/// §2), so determinism must cover it too, not just the `Event` sequence.
fn run_all(commands: &[Command]) -> (Vec<Event>, Vec<EngineSeq>, Option<Price>) {
    let mut engine = Engine::new();
    let mut risk_state = RiskState::new(RiskConfig::default());
    let mut engine_seq = EngineSeq(0);
    let mut events = Vec::new();
    let mut engine_seqs = Vec::new();
    for cmd in commands {
        risk::process_command(
            &mut engine,
            &mut risk_state,
            cmd.clone(),
            &mut engine_seq,
            &mut |seq, e| {
                engine_seqs.push(seq);
                events.push(e)
            },
        );
    }
    (events, engine_seqs, engine.book().last_trade())
}

// Deliberately not the `proptest! { #[test] fn ... }` sugar, and not
// `prop_assert_eq!`: both expand to bare, unhygienic `core::...` paths
// internally (proptest supports no_std, so its macros reference `core`'s
// built-in `panic!`/`file!`/`line!`/etc. directly rather than through
// `std`'s re-exports) -- which this workspace's own crate named `core`
// shadows the moment it's a dependency of the crate under test. Driving
// `TestRunner` manually and asserting with plain `std::assert_eq!` (which
// *is* hygienic here, exactly like every other test in this crate) sidesteps
// the collision entirely rather than renaming `core` workspace-wide for one
// test file.
#[test]
fn full_command_sequence_replays_identically() {
    let mut runner = proptest::test_runner::TestRunner::default();
    let strategy = (
        proptest::collection::vec(op_strategy(), 0..15),
        side_strategy(),
        1u64..=5,
        side_strategy(),
        450u64..=550,
        1u64..=5,
        proptest::collection::vec(op_strategy(), 0..15),
    );

    runner
        .run(
            &strategy,
            |(prefix, market_side, market_qty, band_side, band_price, band_qty, suffix)| {
                let mut commands: Vec<Command> = prefix.into_iter().map(op_to_command).collect();

                // Force the kill switch off regardless of what the random
                // prefix did to it -- otherwise the guaranteed trade below
                // could be rejected instead of executing, silently
                // defeating the guarantee.
                commands.push(Command::KillSwitch { engaged: false });

                // Guaranteed trade: a resting maker, then a taker that
                // crosses it in full, at a price nothing else in this
                // sequence can reach. last_trade never resets to None once
                // set (Book has no such operation), so from this point on
                // for the rest of the sequence it is unconditionally
                // `Some`.
                commands.push(new_order_command(
                    901,
                    1,
                    Side::Sell,
                    GUARANTEED_TRADE_PRICE,
                    5,
                    OrderKind::Limit,
                    Tif::Gtc,
                ));
                commands.push(new_order_command(
                    902,
                    1,
                    Side::Buy,
                    GUARANTEED_TRADE_PRICE,
                    5,
                    OrderKind::Limit,
                    Tif::Gtc,
                ));

                // Unconditionally exercise the last_trade branch of both
                // market_reference_price and price_band_reference -- not
                // merely possible via the random op set, guaranteed on
                // every run.
                commands.push(new_order_command(
                    903,
                    1,
                    market_side,
                    0,
                    market_qty,
                    OrderKind::Market,
                    Tif::Ioc,
                ));
                commands.push(new_order_command(
                    904,
                    1,
                    band_side,
                    band_price,
                    band_qty,
                    OrderKind::Limit,
                    Tif::Gtc,
                ));

                commands.extend(suffix.into_iter().map(op_to_command));

                let (events_a, engine_seqs_a, last_trade_a) = run_all(&commands);
                let (events_b, engine_seqs_b, last_trade_b) = run_all(&commands);

                assert_eq!(events_a, events_b);
                assert_eq!(engine_seqs_a, engine_seqs_b);
                assert_eq!(last_trade_a, last_trade_b);
                Ok(())
            },
        )
        .unwrap();
}
