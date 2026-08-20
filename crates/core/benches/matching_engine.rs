//! Criterion micro-benchmarks on a pre-warmed book (SPEC §9): 10 price
//! levels per side, 20 resting orders per level. In-process, calling
//! `Book` directly -- no socket, no gateway, no risk layer. That's a
//! deliberately different, narrower measurement than `bench`'s HDR
//! harness (stage 7, socket-based, end to end): this isolates the
//! matching engine's own per-operation cost, the HDR harness measures the
//! whole live pipeline under a realistic mixed workload.
//!
//! Every benchmark uses `iter_batched` with a fresh `warm_book()` per
//! batch, not `iter` -- required for correctness, not just style. Cancel,
//! modify, and crossing operations mutate state that later iterations
//! depend on still being there; reusing one book across iterations would
//! make the second iteration onward measure a different (and
//! progressively more depleted) book than the first.

use core::Book;
use core::event::Event;
use core::types::{AccountId, OrderId, Price, Qty, Side};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

const LEVELS_PER_SIDE: u64 = 10;
const ORDERS_PER_LEVEL: u64 = 20;
const ACCOUNT_POOL: u64 = 20;
const WARM_ORDER_QTY: u64 = 5;

/// Best bid 9990, descending in steps of 10 -- `BID_LEVELS[i]` is the i-th
/// best bid.
fn bid_level(i: u64) -> u64 {
    9990 - i * 10
}

/// Best ask 10010, ascending in steps of 10.
fn ask_level(i: u64) -> u64 {
    10010 + i * 10
}

fn discard(_: Event) {}

/// Builds the pre-warmed book and returns every resting order placed,
/// index-addressable in construction order: `[0..200)` is the buy side (10
/// levels x 20 orders, level-major), `[200..400)` is the sell side, same
/// shape. Accounts cycle `1..=ACCOUNT_POOL` across the whole construction
/// sequence (not reset per level/side), so account 1 lands on index 0, 20,
/// 40, ... -- exactly one order per level, all 20 levels, both sides: a
/// deliberately broad `mass_cancel` target.
fn warm_book() -> (Book, Vec<(AccountId, OrderId, Price, Side)>) {
    let mut book = Book::new();
    let mut resting = Vec::with_capacity((LEVELS_PER_SIDE * ORDERS_PER_LEVEL * 2) as usize);
    let mut next_order_id_for_account = vec![0u64; (ACCOUNT_POOL + 1) as usize];
    let mut global_index: u64 = 0;

    for side in [Side::Buy, Side::Sell] {
        for level in 0..LEVELS_PER_SIDE {
            let price = match side {
                Side::Buy => bid_level(level),
                Side::Sell => ask_level(level),
            };
            for _ in 0..ORDERS_PER_LEVEL {
                let account = (global_index % ACCOUNT_POOL) + 1;
                next_order_id_for_account[account as usize] += 1;
                let order_id = next_order_id_for_account[account as usize];

                book.submit_gtc(
                    AccountId(account),
                    OrderId(order_id),
                    side,
                    Price(price),
                    Qty(WARM_ORDER_QTY),
                    &mut discard,
                );
                resting.push((AccountId(account), OrderId(order_id), Price(price), side));
                global_index += 1;
            }
        }
    }

    (book, resting)
}

/// The next order id free on `account_id`, given `warm_book`'s
/// construction (each of the 20 accounts already holds
/// `LEVELS_PER_SIDE * ORDERS_PER_LEVEL * 2 / ACCOUNT_POOL` orders).
fn fresh_order_id(account_id: u64) -> OrderId {
    OrderId(1_000_000 + account_id)
}

fn bench_submit_no_match(c: &mut Criterion) {
    // Appends to an *existing* level (best bid, 9990) -- no crossing, no
    // new B-tree entry.
    c.bench_function("submit_no_match", |b| {
        b.iter_batched(
            warm_book,
            |(mut book, _)| {
                book.submit_gtc(
                    AccountId(999),
                    fresh_order_id(999),
                    Side::Buy,
                    Price(bid_level(0)),
                    Qty(1),
                    &mut discard,
                );
                book
            },
            BatchSize::SmallInput,
        )
    });
}

fn bench_submit_new_level(c: &mut Criterion) {
    // 9985 sits strictly between the best bid (9990) and its neighbour
    // (9980) -- a new B-tree entry, still no crossing (best ask is 10010).
    c.bench_function("submit_new_level", |b| {
        b.iter_batched(
            warm_book,
            |(mut book, _)| {
                book.submit_gtc(
                    AccountId(999),
                    fresh_order_id(999),
                    Side::Buy,
                    Price(9985),
                    Qty(1),
                    &mut discard,
                );
                book
            },
            BatchSize::SmallInput,
        )
    });
}

fn bench_submit_cross_one(c: &mut Criterion) {
    // A sell at 9990 crosses the best bid; qty 5 matches exactly one
    // resting order there (each warmed order is qty 5) and stops.
    c.bench_function("submit_cross_one", |b| {
        b.iter_batched(
            warm_book,
            |(mut book, _)| {
                book.submit_gtc(
                    AccountId(999),
                    fresh_order_id(999),
                    Side::Sell,
                    Price(bid_level(0)),
                    Qty(WARM_ORDER_QTY),
                    &mut discard,
                );
                book
            },
            BatchSize::SmallInput,
        )
    });
}

fn bench_submit_sweep_n(c: &mut Criterion) {
    // A sell at 9900 (the worst/last bid level) crosses every bid level at
    // or above it. qty 250 -- two and a half levels' worth (100 qty per
    // level) -- sweeps multiple levels and multiple orders within them.
    c.bench_function("submit_sweep_n", |b| {
        b.iter_batched(
            warm_book,
            |(mut book, _)| {
                book.submit_gtc(
                    AccountId(999),
                    fresh_order_id(999),
                    Side::Sell,
                    Price(bid_level(LEVELS_PER_SIDE - 1)),
                    Qty(250),
                    &mut discard,
                );
                book
            },
            BatchSize::SmallInput,
        )
    });
}

fn bench_cancel_random(c: &mut Criterion) {
    // Index 10 of the buy side's first level (9990) -- the middle of a
    // 20-order queue, not the head or tail, so this exercises the O(1)
    // unlink's general case rather than a boundary special-case.
    c.bench_function("cancel_random", |b| {
        b.iter_batched(
            warm_book,
            |(mut book, resting)| {
                let (account_id, order_id, ..) = resting[10];
                book.cancel(account_id, order_id, &mut discard);
                book
            },
            BatchSize::SmallInput,
        )
    });
}

fn bench_modify_decrease(c: &mut Criterion) {
    // Index 210: the middle of the sell side's first level (10010). Same
    // price, qty 5 -> 2 -- a decrease retains queue priority.
    c.bench_function("modify_decrease", |b| {
        b.iter_batched(
            warm_book,
            |(mut book, resting)| {
                let (account_id, order_id, price, _side) = resting[210];
                book.modify(account_id, order_id, price, Qty(2), &mut discard);
                book
            },
            BatchSize::SmallInput,
        )
    });
}

fn bench_modify_increase(c: &mut Criterion) {
    // Same target, opposite direction: qty 5 -> 10 -- an increase loses
    // queue priority (re-enters at the back of its level).
    c.bench_function("modify_increase", |b| {
        b.iter_batched(
            warm_book,
            |(mut book, resting)| {
                let (account_id, order_id, price, _side) = resting[210];
                book.modify(account_id, order_id, price, Qty(10), &mut discard);
                book
            },
            BatchSize::SmallInput,
        )
    });
}

fn bench_mass_cancel(c: &mut Criterion) {
    // Account 1 lands on construction indices 0, 20, 40, ... 380 -- one
    // order per level, all 20 levels, both sides: a broad, realistic
    // mass-cancel target, not a single-level edge case.
    c.bench_function("mass_cancel", |b| {
        b.iter_batched(
            warm_book,
            |(mut book, _)| {
                book.mass_cancel(AccountId(1), &mut discard);
                book
            },
            BatchSize::SmallInput,
        )
    });
}

criterion_group!(
    benches,
    bench_submit_no_match,
    bench_submit_new_level,
    bench_submit_cross_one,
    bench_submit_sweep_n,
    bench_cancel_random,
    bench_modify_decrease,
    bench_modify_increase,
    bench_mass_cancel,
);
criterion_main!(benches);
