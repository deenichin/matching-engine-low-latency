# Implementation Plan

Staged build per SPEC.md §11. The gate for each stage is its exit criteria — not
elapsed time. Do not start the next stage without explicit go-ahead. Commit at
stage boundaries only, never mid-stage.

Stages 0–8 are the **Core slice**, scoped to roughly three hours of
implementation on top of the design and specification work that precedes it.
Stages 9–11 are the **Extension slice**, deliberately beyond that, each with its
rationale recorded. The division was decided before any code existed.

---

## Stage 0 — Workspace and domain types

Environment setup verified the toolchain with a single generic crate — no
workspace, no crate boundaries, nothing SPEC-shaped. This stage is the first
real design work: turning that placeholder into the architecture SPEC §4
describes, plus the types the rest of the system is expressed in.

- `Cargo.toml` — convert to `[workspace]`, members: `core`, `wire`, `risk`,
  `gateway`, `marketdata`, `bin`, `bench`, matching the dependency direction in
  SPEC §4 (each crate's own `Cargo.toml` depends only downward)
- `crates/core/src/types.rs` — `Price`, `Qty`, `OrderId`, `AccountId`,
  `EngineSeq`, `StreamSeq`, `Side`, `OrderKind`, `Tif`, with the unit
  semantics from SPEC §2 in doc comments (price is quote-per-base-lot; qty is
  base lots)
- `crates/core/src/event.rs` — `Command`, `Event`
- `crates/core/src/error.rs` — the full `RejectReason` taxonomy from SPEC §5,
  each variant with its distinct wire value
- `crates/{wire,risk,gateway,marketdata}/src/lib.rs` — compiling skeletons
- `crates/bin/src/main.rs`, `crates/bench/src/main.rs` — stubs
- Named constants: `TICK_SIZE_CENTS`, `LOT_SIZE`, `SYMBOL`, `MAX_OPEN_ORDERS`,
  `MAX_NOTIONAL_TICKS`, `PRICE_BAND_PCT`
- `risk.toml` carrying the SPEC §5 defaults
- `check.sh` upgraded to `--workspace` throughout
- `Dockerfile` and `docker-compose.yml` rewritten for the real binaries and the
  two UDS socket paths (`/run/engine`, named volume)

**Exit:** `./check.sh` green; `cargo tree -p core` shows no dependencies;
`docker compose up --build` runs the new scaffold. Commit `feat: workspace,
domain types, and reject taxonomy`.

---

## Stage 1 — `core`: the book, account index, and matching

Build order (each step verified before the next):

1. `Arena` — `Vec<Option<Node>>` + free list. `Node` carries id, side, price,
   qty, account, `prev`/`next` as `u32` slots, and `acct_idx: u32`. Check the
   struct size against the 64-byte cache-line target before proceeding.
2. `Level` — head/tail, cached `total_qty`, `count`
3. `BTreeMap<Price, Level>` per side; `HashMap<(AccountId, OrderId), u32>`
   order index — keyed on the pair, since `OrderId` is only unique per account
   (SPEC §2, §4)
4. `AccountEntry { slots: Vec<u32>, notional: u128 }` and
   `HashMap<AccountId, AccountEntry>`. `slots` reserved to `MAX_OPEN_ORDERS` on
   first sight of an account. Maintenance lives in exactly two places: `rest()`
   pushes and sets `acct_idx`; `unlink()` does `swap_remove` and fixes the moved
   element's `acct_idx`.
5. `assert_invariants()` — every bullet in SPEC §6, **including the account
   index cross-checks in both directions**, written before the logic it checks
6. GTC submit: sweep, FIFO, maker-price execution, rest remainder
7. Cancel: O(1) unlink from level and account index, ownership check, no-oracle
   rejection
8. IOC, FOK (account-aware precheck per SPEC §6), PostOnly, Market
9. Modify: the full SPEC §2 table; `Replaced` before `Filled` on a crossing
   modify
10. MassCancel: walk the account's `slots` in resting order, unlink each
11. STP cancel-resting, applied to submits and crossing modifies

Scenario tests: FIFO within a level; sweep across levels; maker-price execution;
partial fill rests remainder; cancel from head/middle/tail; cancel empties a
level; **two different accounts submitting the same numeric OrderId both
succeed independently** (the case the (AccountId, OrderId) key exists to fix —
this is the single most important new test in this stage); cancel and modify
rejected for wrong account, identically to unknown id;
IOC discards; FOK all-or-nothing; FOK counts only crossable depth; FOK rejects
when all crossable depth is the aggressor's own; PostOnly rejects a crossing
order; PostOnly rests when not crossing; PostOnly crossing only its own
resting order cancels that resting order for real (emits `Cancelled`) and
rests, instead of rejecting `WouldCross`; **PostOnly crossing self-owned
depth *and* foreign depth beyond it cancels the self-owned depth for real
and still rejects `WouldCross` against the foreign depth, with the
cancellation not reversed** (the eager-vs-atomic decision in SPEC §2 — this
is the case the quantity-conservation property test's shrunk failure
found); market order partial-fills and
discards; modify-decrease retains priority; modify-increase loses it;
`modify_price_change_crosses_emits_replaced_then_filled` (asserts exact event
sequence, not just presence); modify price change without crossing; STP cancels
resting side; STP continues past a cancelled maker; STP across consecutive
same-account levels; book never locks.

Account-index tests specifically: mass-cancel emission order is deterministic
across repeated identical command sequences (not arrival order — `swap_remove`
reorders, per SPEC §2); mass-cancel on an account with no orders is a no-op, not
an error; mass-cancel
leaves other accounts untouched; `acct_idx` stays correct after a `swap_remove`
from the middle (cancel the middle order of three, then mass-cancel the rest and
assert both survivors are emitted); notional updates on rest, cancel, fill, and
modify in both directions.

Property test: quantity conservation over submit/cancel/modify/mass-cancel.

**Exit:** all §6 layers green for core behaviour. Commit `feat: matching core`.

---

## Stage 2 — `wire`: protocol

1. Message tag enum, one `u8` per type, distinct values
2. Fixed-layout encode/decode for each inbound type into `core::Command`
3. Fixed-layout encode for each outbound type from `core::Event`
4. Framing: buffer, extract complete messages, retain partial remainder
5. Validation: length vs. tag, enum range, nonzero price/qty

Tests: round-trip every message type; truncated frame; unknown tag;
out-of-range enum; zero qty; one frame split across two reads; two frames in one
read; trailing partial after several complete.

**Exit:** every type round-trips; every malformed case rejects with its specific
reason code. Commit `feat: binary wire protocol`.

---

## Stage 3 — `gateway` + `bin`: end-to-end

1. `Transport` trait; `UdsTransport` implementing it
2. Stale-socket unlink on bind, cleanup on shutdown
3. Accept loop, `ConnId` assignment, per-connection read buffers
4. Bounded `(ConnId, Command)` channel to the matching thread
5. Matching thread: busy-spin, sole `Engine::apply` caller
6. Bounded `(ConnId, Event)` return channel — **individual events, never
   `Vec<Event>`** (SPEC §4); gateway writes each report as it arrives
7. `bin` wires it together; graceful shutdown

The market-data thread (SPEC §4) arrives in stage 5 along with its socket. This
stage builds two of the three threads; leave the market-data channel's send side
stubbed rather than wiring a third thread prematurely.

**Exit:** an order sent over the socket produces a correct execution report; a
second connection is served independently. Commit `feat: UDS gateway and
matching thread`.

---

## Stage 4 — `risk`

1. Kill switch, drain policy, checked inside the matching loop before `apply`
2. Per-account open-order count and notional read from `AccountEntry` (O(1),
   already maintained in stage 1 — this stage adds the *checks*, not the
   bookkeeping)
3. Price band vs. last-trade reference, book-mid fallback, skip on empty book,
   skipped (not zero) for Market
4. Notional for Market orders is resolved **differently from the band**: last
   trade price, else the best price on the side the order sweeps (best ask
   for a buy, best bid for a sell) — never the wire `price = 0`, and never
   the band's two-sided mid, which a one-sided book can't produce. Reject
   `NotFullyFillable` only if neither exists (nothing to sweep at all).
5. `risk.toml` loading with baked-in defaults
6. Control-plane messages: `KillSwitch`, `Snapshot`

Tests: kill switch rejects new entry; kill switch still allows cancel of resting
orders (drain); a command already queued when the switch fires is still
rejected; CancelReplace during drain that only decreases quantity is allowed;
a CancelReplace during drain that increases quantity or changes price is
blocked with `KillSwitchActive`, identically to a blocked new order; max
open orders breach at exactly the cap boundary; max notional
breach; notional arithmetic near `u64::MAX` does not wrap (the `u128` case);
price band above and below; band skipped on an empty book; band skipped on a
one-sided book (bids only, no asks — mid is undefined); band uses last trade in
preference to mid once a trade has occurred; band does not apply to Market
orders; **Market order notional uses the reference price, not the wire
price = 0 — a large Market order against thin reference liquidity breaches the
cap** (this is the test that would have caught the original gap); **a Market
order into a one-sided book with real depth on the side it sweeps fills
normally and is not rejected for notional** (the regression test for the
bug the reference-price resolution was corrected to fix); Market rejected
with NotFullyFillable only when no depth exists on the side it sweeps and no
trade has occurred; modify to zero quantity rejected with `ZeroQuantity`;
each with its distinct reason code.

**Exit:** every control has a scenario test naming its reason code. Commit
`feat: risk and operational controls`.

---

## Stage 5 — `marketdata`

1. Second UDS listener on its **own thread** (SPEC §4), subscriber management
2. `BookUpdate` on every book change; `Trade` on every execution
3. Independent `StreamSeq` counter for this stream, per SPEC §2 — **not**
   shared with execution reports' counter, and not `EngineSeq`
4. Bounded broadcast, drop-oldest, never blocking the matching thread
5. Gap detection hooks on the subscriber side

Tests: subscriber receives both streams; **each stream's `StreamSeq` is
monotonic and gapless under normal operation on its own** — an execution-report
event must never advance market data's counter or vice versa; a deliberately
slow subscriber drops rather than stalling the matcher; a deliberately slow
subscriber does not stall **order entry** either — this is the specific
coupling the separate thread exists to prevent, so assert order-entry
throughput is unaffected while a subscriber is blocked; a drop is detectable as
a gap in the *affected stream's own* `StreamSeq`, without a phantom gap
appearing on the unaffected stream.

**Exit:** both streams verified; backpressure tested. Commit `feat: market data
streams`.

**Known gap:** an isolated marketdata-only backpressure test (subscribers fed
directly via the internal channel, bypassing the matching thread) was
attempted, stalled under sustained load for an undiagnosed reason, and was
deliberately dropped rather than chased further. The four end-to-end tests in
`crates/bin/tests/market_data.rs` — exercising the same `subscriber.rs`/
`queue.rs` code through the real daemon — satisfy the original requirement in
full (fast/slow isolation, execution-report/market-data `StreamSeq`
independence, and order-entry/matching progress under a stalled subscriber).
The isolated case remains a known gap, not a hidden one.

---

## Stage 6 — Determinism

1. Recording: decoded post-gateway-validation `Command` sequence to a file
   (before risk — see SPEC §7)
2. Replay: read file, feed the sequence through risk-then-matching exactly as
   live traffic does; bypass only the socket and framing layer
3. `--exclude-timestamps` flag, documented
4. Byte-identical comparison of the full outbound stream — **including
   rejections**: a command risk-rejected on the original run must be
   risk-rejected identically on replay, not silently accepted
5. Determinism property test over the full operation set, mass-cancel included
6. Concurrent-submission priority test: multiple client threads over real
   sockets, asserting fills reflect strict arrival order at the matcher
7. **`last_trade` (and any other `Book`-internal state with no direct
   event-stream representation) needs its own explicit replay verification,
   decided before this stage is built, not after.** A byte-identical
   outbound-events comparison does not by itself prove `last_trade` replayed
   correctly — it can diverge silently if nothing in the recorded sequence
   exercises a path that reads it. Two approaches: compare `last_trade` (and
   any other such state) directly, not just the outbound stream; or build the
   operation set so every `last_trade`-dependent branch is guaranteed to
   produce an observable, divergence-sensitive event. State which one this
   stage takes before implementing it.
8. **The determinism property test's operation set must deliberately include
   a Market order or price-banded order submitted after at least one trade
   has occurred**, specifically to exercise the `last_trade` branch of both
   the price-band and Market-notional resolution, not just their
   depth-based fallback paths. Being "in the op set" is not enough --
   `quantity_conservation`'s op set already includes `Market` and never
   exercised `RiskState` at all, since `RiskState` didn't exist when that
   test was written (stage 1, before stage 4). The op set has to reach the
   specific branch being tested, not merely include the command type that
   could reach it.

Ship a recorded stream and the replay command in the repo.

**Exit:** replay byte-identical; concurrency test green. Commit `feat:
deterministic record and replay`.

---

## Stage 7 — Benchmarks

1. Criterion micro-benchmarks on a warm book (SPEC §9 list, `mass_cancel`
   included)
2. HDR histogram harness: ingress → execution report, p50/p99/p99.9/p99.99
3. Coordinated-omission handling: measure against intended send time at a fixed
   offered rate
4. Warmup phase, excluded and counted
5. Burst workload: 1M messages, default 70/20/10 mix, configurable
6. Sustained throughput alongside the distribution
7. **Two** allocation tests behind the `count-allocations` feature per SPEC §6:
   `hot_path_allocates_nothing` (existing levels only, asserts zero) and
   `level_churn_allocation_is_bounded` (records allocations per 1,000
   operations across a realistic price band). Plus the `cargo tree` check in the
   normal test run proving the dependency is absent by default
8. `BENCH.md` tail interpretation must reference the churn figure — an
   occasional B-tree node split on a new-tick submit is a concrete named tail
   source, and the per-1,000-operations rate is what answers the near-zero
   target

**Exit:** numbers in `BENCH.md`; zero-alloc test green. Commit `test: HDR
latency harness and benchmarks`.

---

## Stage 8 — Documentation

`README.md`: architecture description and crate DAG; wire protocol tables (field,
offset, width, type, endianness); how to run, benchmark, and replay; matching /
STP / cancel-replace / market-order policies; reason-code taxonomy table;
risk-control configuration; the tooling table from SPEC §9 with the rationale for
each crate and for the optional-dependency gating; the ~1 page microstructure
write-up (matching rules and why; when a CLOB is the right primitive for a
ZK-settled crypto-native venue versus batch auction / RFQ / hybrid; one honest
limitation and how it would evolve); assumptions and tradeoffs; known
limitations.

`BENCH.md`: methodology, machine spec, warmup and sample counts, pinning state,
and a paragraph on where the tail comes from.

Verify `docker compose up --build` from a clean clone.

**Exit:** all doc deliverables complete; `./check.sh` green. Commit `docs:
README, BENCH, microstructure write-up`.

---

# Extension slice — beyond the core window

## Stage 9 — CPU pinning

Pin the matching thread to a dedicated core. Re-run the HDR harness with and
without pinning under identical workload; record both distributions in
`BENCH.md`.

**Why beyond core:** cheap and directly relevant to prior kernel-bypass and
bare-metal work, and it produces a measured before/after rather than a claim.

**Exit:** both distributions recorded. Commit `perf: pin matching thread to a
dedicated core`.

---

## Stage 10 — DPDK path documentation

README section: how a `DpdkTransport` would implement the existing `Transport`
trait; where `unsafe` is confined; the RAII mbuf wrapper reconciling DPDK's
manual pool management with Rust ownership; what does *not* change (`core`,
`wire`, `risk`, matching logic).

**Why beyond core:** demonstrates the abstraction survives a move to
kernel-bypass networking, with the trait boundary already real rather than
promised.

**Exit:** section written. Commit `docs: kernel-bypass transport path`.

---

## Stage 11 — Hugepages and NUMA write-up

README section: what each would buy, why neither is measured here (hugepages
need host-level `hugetlbfs` reservation, conflicting with zero-setup startup;
NUMA needs multi-socket bare metal), and why a number produced on unknown
hardware would be noise presented as evidence.

**Why beyond core:** completes the latency-engineering story honestly.

**Exit:** section written. Commit `docs: hugepages and NUMA considerations`.

---

## Cut order if time runs short

Stage 11 → stage 10 → stage 9 → stage 7's criterion micro-benchmarks (keeping
the HDR hot-path measurement, which is a hard requirement).

**Never cut stage 8.**
