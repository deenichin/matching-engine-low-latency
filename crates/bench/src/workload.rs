//! Burst workload generation (SPEC §9): the account/order-id scheme, the
//! price structure new orders rest at, and the shared "known resting
//! orders" pool cancels draw from -- populated by the reader thread as it
//! observes replies, drained by the sender thread as it picks cancel
//! targets. The same shape as gateway's `resting_conn` table
//! (`crates/gateway/src/matching.rs`): a live association updated as
//! orders start and stop resting, not recomputed from scratch each time.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use core::event::{Command, Event};
use core::types::{AccountId, OrderId, OrderKind, Price, Qty, Side, Tif};

/// Round-robins new-order accounts across this many distinct `AccountId`s.
const ACCOUNT_POOL: u64 = 256;

/// Five resting levels per side, not fewer -- a large-qty sweep needs
/// several levels to cross through for `submit_sweep_n`-style traffic to
/// be meaningful under live measurement (see `Kind::CrossLimitSweep`
/// below). Every price here is always within +-10% of any `last_trade`
/// this workload can produce (the full range is ~1% wide), so the
/// production price band -- deliberately left unrelaxed in
/// `risk-bench.toml` -- never rejects legitimate workload traffic.
const BUY_LEVELS: [u64; 5] = [9950, 9960, 9970, 9980, 9990];
const SELL_LEVELS: [u64; 5] = [10010, 10020, 10030, 10040, 10050];

const NEW_ORDER_QTY: u64 = 5;

/// Large enough to plausibly cross several of the 5 levels on the swept
/// side.
const SWEEP_QTY: u64 = 300;

/// The mix, as three percentages that must sum to 100 (SPEC §9: default
/// 70/20/10 new/cancel/cross, configurable via flag).
#[derive(Debug, Clone, Copy)]
pub struct Mix {
    pub new_pct: u32,
    pub cancel_pct: u32,
    pub cross_pct: u32,
}

impl Mix {
    pub fn validate(self) -> Result<Self, String> {
        let total = self.new_pct + self.cancel_pct + self.cross_pct;
        if total != 100 {
            return Err(format!(
                "--new-pct + --cancel-pct + --cross-pct must sum to 100, got {total}"
            ));
        }
        Ok(self)
    }
}

/// A small, dependency-free deterministic PRNG (xorshift64*) -- workload
/// shuffling doesn't need a `rand` crate, and SPEC §9 requires justifying
/// every dependency individually (the same reasoning as stage 4's
/// hand-rolled `risk.toml` parser).
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..n`. `n` is always small and fixed in this module
    /// (100, 5, 2), so the modulo bias is negligible and not worth a
    /// rejection-sampling loop.
    fn next_range(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// One aggressive-crossing order's realized shape: half `Market` (always
/// discards any remainder, per SPEC §2 -- exercises the price-band-exempt
/// path, which is *why* the band doesn't need relaxing here), half a
/// large-qty GTC `Limit` priced at the far edge of the opposite side's
/// level range (crosses several levels, then rests what's left). An
/// all-`Market` crossing bucket could never produce
/// `Accepted{resting_qty>0}` after a partial sweep, since `Market` always
/// behaves as IOC -- this split is what makes that path actually appear
/// in the HDR measurement.
enum Kind {
    New,
    Cancel,
    CrossMarket,
    CrossLimitSweep,
}

fn kind_for_roll(mix: Mix, roll: u32) -> Kind {
    if roll < mix.new_pct {
        Kind::New
    } else if roll < mix.new_pct + mix.cancel_pct {
        Kind::Cancel
    } else {
        let cross_offset = roll - mix.new_pct - mix.cancel_pct;
        if cross_offset * 2 < mix.cross_pct {
            Kind::CrossMarket
        } else {
            Kind::CrossLimitSweep
        }
    }
}

pub struct Workload {
    mix: Mix,
    next_order_id: AtomicU64,
    next_account: AtomicU64,
    resting_pool: Mutex<VecDeque<(AccountId, OrderId)>>,
    rng: Mutex<Rng>,
}

impl Workload {
    pub fn new(mix: Mix, seed: u64) -> Self {
        Self {
            mix,
            next_order_id: AtomicU64::new(1),
            next_account: AtomicU64::new(0),
            resting_pool: Mutex::new(VecDeque::new()),
            // Never zero -- xorshift's fixed point.
            rng: Mutex::new(Rng(seed | 1)),
        }
    }

    fn fresh_account(&self) -> AccountId {
        let n = self.next_account.fetch_add(1, Ordering::Relaxed) % ACCOUNT_POOL;
        AccountId(n + 1)
    }

    fn fresh_order_id(&self) -> OrderId {
        OrderId(self.next_order_id.fetch_add(1, Ordering::Relaxed))
    }

    /// The next command to send, and the `(AccountId, OrderId)` key its
    /// reply will carry -- used by the harness to correlate the reply back
    /// to this send's intended time.
    pub fn next_command(&self) -> ((AccountId, OrderId), Command) {
        let roll = {
            self.rng
                .lock()
                .expect("workload rng mutex poisoned")
                .next_range(100)
        } as u32;

        match kind_for_roll(self.mix, roll) {
            Kind::New => {
                let (side_idx, level_idx) = {
                    let mut rng = self.rng.lock().expect("workload rng mutex poisoned");
                    (rng.next_range(2), rng.next_range(BUY_LEVELS.len() as u64))
                };
                let (side, price) = if side_idx == 0 {
                    (Side::Buy, BUY_LEVELS[level_idx as usize])
                } else {
                    (Side::Sell, SELL_LEVELS[level_idx as usize])
                };
                let account_id = self.fresh_account();
                let order_id = self.fresh_order_id();
                let cmd = Command::NewOrder {
                    account_id,
                    order_id,
                    side,
                    price: Price(price),
                    qty: Qty(NEW_ORDER_QTY),
                    kind: OrderKind::Limit,
                    tif: Tif::Gtc,
                    client_ts: 0,
                };
                ((account_id, order_id), cmd)
            }
            Kind::Cancel => {
                let target = self
                    .resting_pool
                    .lock()
                    .expect("resting pool mutex poisoned")
                    .pop_front();
                match target {
                    Some((account_id, order_id)) => {
                        let cmd = Command::CancelOrder {
                            account_id,
                            order_id,
                        };
                        ((account_id, order_id), cmd)
                    }
                    // The pool is momentarily empty (early in the run, or
                    // cancels briefly outpacing new resting orders) --
                    // fall back to a new order rather than stalling the
                    // fixed schedule waiting for something to cancel.
                    None => self.new_order_fallback(),
                }
            }
            Kind::CrossMarket => {
                let side = if self
                    .rng
                    .lock()
                    .expect("workload rng mutex poisoned")
                    .next_range(2)
                    == 0
                {
                    Side::Buy
                } else {
                    Side::Sell
                };
                let account_id = self.fresh_account();
                let order_id = self.fresh_order_id();
                let cmd = Command::NewOrder {
                    account_id,
                    order_id,
                    side,
                    price: Price(0), // unused for Market (SPEC §2)
                    qty: Qty(SWEEP_QTY),
                    kind: OrderKind::Market,
                    tif: Tif::Ioc, // Market always behaves as IOC regardless (SPEC §2)
                    client_ts: 0,
                };
                ((account_id, order_id), cmd)
            }
            Kind::CrossLimitSweep => {
                let side = if self
                    .rng
                    .lock()
                    .expect("workload rng mutex poisoned")
                    .next_range(2)
                    == 0
                {
                    Side::Buy
                } else {
                    Side::Sell
                };
                // The far edge of the opposite side's level range: a buy
                // sweep prices at the worst (highest) sell level, a sell
                // sweep at the worst (lowest) buy level, so it can cross
                // through all 5 levels if there's enough resting depth.
                let price = match side {
                    Side::Buy => *SELL_LEVELS.last().expect("SELL_LEVELS is non-empty"),
                    Side::Sell => *BUY_LEVELS.first().expect("BUY_LEVELS is non-empty"),
                };
                let account_id = self.fresh_account();
                let order_id = self.fresh_order_id();
                let cmd = Command::NewOrder {
                    account_id,
                    order_id,
                    side,
                    price: Price(price),
                    qty: Qty(SWEEP_QTY),
                    kind: OrderKind::Limit,
                    tif: Tif::Gtc,
                    client_ts: 0,
                };
                ((account_id, order_id), cmd)
            }
        }
    }

    fn new_order_fallback(&self) -> ((AccountId, OrderId), Command) {
        let (side_idx, level_idx) = {
            let mut rng = self.rng.lock().expect("workload rng mutex poisoned");
            (rng.next_range(2), rng.next_range(BUY_LEVELS.len() as u64))
        };
        let (side, price) = if side_idx == 0 {
            (Side::Buy, BUY_LEVELS[level_idx as usize])
        } else {
            (Side::Sell, SELL_LEVELS[level_idx as usize])
        };
        let account_id = self.fresh_account();
        let order_id = self.fresh_order_id();
        let cmd = Command::NewOrder {
            account_id,
            order_id,
            side,
            price: Price(price),
            qty: Qty(NEW_ORDER_QTY),
            kind: OrderKind::Limit,
            tif: Tif::Gtc,
            client_ts: 0,
        };
        ((account_id, order_id), cmd)
    }

    /// Updates the resting pool from an observed reply -- the same
    /// add-on-rest, remove-on-stop-resting shape as gateway's
    /// `resting_conn` table.
    pub fn observe(&self, event: &Event) {
        let mut pool = self
            .resting_pool
            .lock()
            .expect("resting pool mutex poisoned");
        match *event {
            Event::Accepted {
                account_id,
                order_id,
                resting_qty,
            } if resting_qty.0 > 0 => {
                pool.push_back((account_id, order_id));
            }
            Event::Cancelled {
                account_id,
                order_id,
            } => {
                pool.retain(|&id| id != (account_id, order_id));
            }
            Event::Filled {
                account_id,
                order_id,
                resting_qty,
                ..
            } if resting_qty.0 == 0 => {
                pool.retain(|&id| id != (account_id, order_id));
            }
            _ => {}
        }
    }
}
