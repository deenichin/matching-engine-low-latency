# matching-engine

Single-symbol central limit order book with exchange-style connectivity,
deterministic replay, tail-latency measurement, and risk controls. Full
requirements: [SPEC.md](SPEC.md). Staged build history: [PLAN.md](PLAN.md).
Benchmark methodology and numbers: [BENCH.md](BENCH.md).

## Architecture

Six library crates plus two binary crates, dependencies pointing strictly
downward (verifiable: `cargo tree -p core` is empty):

```
core   (zero dependencies — Engine, Book, Arena, Level, AccountEntry, domain types)
 ^
 |-- wire        (encode/decode/framing; decodes straight into core types)
 |-- risk        (kill switch, per-account limits, price band)
 |     ^
 |     |-- gateway (Transport trait, UDS impl, connection mgmt, validation)
 |-- marketdata  (top-of-book + trade publishing; depends on wire, core only)

bin    (the daemon: `engine` binary wires gateway + risk + marketdata + core
        together; `replay` binary reads a recorded stream)
bench  (benchmark client: a real client of the order-entry socket)
```

`core` knows nothing about sockets, threads, or bytes. `wire` decodes
directly into `core::Command`/`core::Event` — no parallel type hierarchy.
`gateway` depends on `wire`+`risk`+`core`; `marketdata` deliberately does
**not** depend on `gateway` (it duplicates the small stale-socket-unlink
logic rather than share `gateway`'s `Transport`, which is scoped to order
entry).

### Thread topology

Three threads, each owning its own socket(s) — no socket is ever touched by
more than one thread:

- **Gateway thread** — owns the order-entry socket. Accepts connections,
  assigns a `ConnId`, frames and validates bytes, pushes `(ConnId, Command)`
  onto a bounded channel. Also owns the write side: drains a
  `(ConnId, Event)` return channel and writes each execution report to the
  connection it belongs to, one event per write — never a batched
  `Vec<Event>` (a channel of `Vec<Event>` looks bounded at the channel
  level, but the payload allocates fresh on every dispatch that produces
  one or more events, i.e. every submit/cancel/modify).
- **Matching thread** — the sole caller of `Engine::apply`. Busy-spins on
  the command channel rather than blocking, to avoid futex-wake and
  context-switch jitter in the tail. Per iteration: check the kill switch,
  check risk (`risk::process_command`), apply, dispatch resulting events
  individually to the gateway's return channel and the market-data channel.
  `ConnId` passes through opaquely — `Engine` never sees it, which is what
  keeps `core` free of any connection concept.
- **Market-data thread** — owns the market-data socket and its subscribers.
  Drains a bounded channel fed by the matching thread and fans out to each
  subscriber independently. A separate thread specifically so a slow
  subscriber blocking on `write()` degrades only its own feed, never
  order-entry reads.

Routing a reply is not "answer whoever sent the triggering command": a
fill, an STP cancellation, or a `MassCancel` can produce events for a
resting order that belongs to a different connection than the one that
triggered the match. The gateway thread keeps a `(AccountId, OrderId) ->
ConnId` table, updated as orders start and stop resting.

`EngineSeq` and `StreamSeq` are different things, assigned at different
points by different owners. `core::Event` carries neither field itself.
`StreamSeq` — one independent counter *per outbound stream*, execution
reports and market data each having their own — is stamped downstream, by
whichever thread owns the destination stream, at its own encode point.
`EngineSeq` — monotonic across every command the *system* processes,
including one risk-rejected before it ever reaches `Engine::apply` — is
assigned earlier and on the matching thread itself: `risk::process_command`
increments it exactly once per command, before checking risk or calling
`Engine::apply`, which is deliberately the single increment site so replay
(which calls the same function) can never disagree with the live matching
thread about it. See "Wire protocol" below for what's actually on the wire.

### `Transport` trait

```rust
pub trait Transport: Sized {
    type Connection: Read + Write;
    fn bind(path: &Path) -> std::io::Result<Self>;
    fn accept(&self) -> std::io::Result<Self::Connection>;
}
```

`UdsTransport` is the only implementation today (Unix Domain Socket,
`SOCK_STREAM`, stale-socket-file unlink on bind, cleaned up on `Drop`).
Ingress sits behind this trait so transport is a compile-time choice, not
an architectural commitment — a DPDK implementation would be a second
`impl`, with `unsafe` FFI confined to that module behind an RAII mbuf
wrapper; nothing in `core`, `wire`, or `risk` would change.

**Why UDS, not TCP, for this build.** The system is scoped to a single
process on a single host. UDS removes the TCP/IP stack from the
measurement path, so benchmark numbers describe the engine rather than the
loopback stack, and it gives kernel-enforced access control via filesystem
permissions plus kernel-verified peer identity via `SO_PEERCRED`. A
production venue would use TCP (FIX or a binary equivalent) for order entry
since clients are remote, UDP multicast for market-data fan-out since
unicast doesn't scale to many subscribers, and likely shared-memory ring
buffers for the internal gateway↔matcher hop. UDS is the right choice under
*this* build's constraints, not a general claim about production
architecture.

---

## Wire protocol

SBE-inspired, fixed-layout, little-endian (native on x86-64 and ARM64).
Byte 0 is always a `u8` tag; the tag alone determines total message length,
so there's no separate length prefix. All multi-byte integers are
little-endian `u64` unless noted. Booleans are one byte, `0`/`1`.

### Inbound (tag values 1–6)

**`NewOrder`** (tag 1, 44 bytes)

| field | offset | width | type |
|---|---|---|---|
| tag | 0 | 1 | u8 (=1) |
| order_id | 1 | 8 | u64 |
| account_id | 9 | 8 | u64 |
| side | 17 | 1 | u8 (0=Buy, 1=Sell) |
| price | 18 | 8 | u64 (unused, must still be present, if kind=Market) |
| qty | 26 | 8 | u64 |
| order_kind | 34 | 1 | u8 (0=Limit, 1=Market) |
| tif | 35 | 1 | u8 (0=Gtc, 1=Ioc, 2=Fok, 3=PostOnly) |
| client_ts | 36 | 8 | u64 |

**`CancelOrder`** (tag 2, 17 bytes)

| field | offset | width | type |
|---|---|---|---|
| tag | 0 | 1 | u8 (=2) |
| order_id | 1 | 8 | u64 |
| account_id | 9 | 8 | u64 |

**`CancelReplace`** (tag 3, 33 bytes)

| field | offset | width | type |
|---|---|---|---|
| tag | 0 | 1 | u8 (=3) |
| order_id | 1 | 8 | u64 |
| account_id | 9 | 8 | u64 |
| new_price | 17 | 8 | u64 |
| new_qty | 25 | 8 | u64 |

**`MassCancel`** (tag 4, 9 bytes)

| field | offset | width | type |
|---|---|---|---|
| tag | 0 | 1 | u8 (=4) |
| account_id | 1 | 8 | u64 |

**`KillSwitch`** (tag 5, 2 bytes)

| field | offset | width | type |
|---|---|---|---|
| tag | 0 | 1 | u8 (=5) |
| engaged | 1 | 1 | bool |

**`Snapshot`** (tag 6, 1 byte — tag only, no body)

### Outbound (tag values 10–19)

Every outbound message's second field is `stream_seq` (8 bytes, `u64`) —
this stream's own counter, independent of the other outbound stream's.

**`EngineSeq` vs `StreamSeq`, plainly**: `engine_seq` is monotonic across
every command the *system* processes — including one risk-rejected before
it ever reaches `Engine::apply` — assigned once, in one place
(`risk::process_command`), so the live matching thread and replay can never
disagree about it. `stream_seq` is per outbound *stream* — one counter for
execution reports, a separate one for market data — and exists purely for
gap detection (§8): did this particular stream lose a message. They count
different things, they're both on the wire, and neither substitutes for
the other. `engine_seq` appears only on the five execution-report types
below (`Accepted`/`Rejected`/`Filled`/`Cancelled`/`Replaced`); market data
(`Trade`/`BookUpdate`) and the `Snapshot*` response types carry
`stream_seq` only, since neither is tied to one command's position in the
system-wide sequence.

**`Accepted`** (tag 10, 41 bytes)

| field | offset | width | type |
|---|---|---|---|
| tag | 0 | 1 | u8 (=10) |
| stream_seq | 1 | 8 | u64 |
| engine_seq | 9 | 8 | u64 |
| account_id | 17 | 8 | u64 |
| order_id | 25 | 8 | u64 |
| resting_qty | 33 | 8 | u64 |

**`Rejected`** (tag 11, 34 bytes)

| field | offset | width | type |
|---|---|---|---|
| tag | 0 | 1 | u8 (=11) |
| stream_seq | 1 | 8 | u64 |
| engine_seq | 9 | 8 | u64 (`0` sentinel — see below) |
| account_id | 17 | 8 | u64 |
| order_id | 25 | 8 | u64 |
| reason | 33 | 1 | u8 (`RejectReason`, see table below) |

`engine_seq` is `0` on a `Rejected` when the reject never became a
`Command` at all — a malformed frame or unknown tag, rejected by the
gateway's reader before `wire::decode_command` ever succeeds, so it never
reaches `risk::process_command`. `EngineSeq` starts at `1` for the first
command that actually enters the system (the counter is incremented
before it's read), so `0` is otherwise never produced — it unambiguously
means "rejected at the wire layer, before this ever became a command,"
and a client tracking its own command flow can treat it as "did not
consume a sequence number."

**`Filled`** (tag 12, 59 bytes)

| field | offset | width | type |
|---|---|---|---|
| tag | 0 | 1 | u8 (=12) |
| stream_seq | 1 | 8 | u64 |
| engine_seq | 9 | 8 | u64 |
| account_id | 17 | 8 | u64 |
| order_id | 25 | 8 | u64 |
| side | 33 | 1 | u8 (0=Buy, 1=Sell) |
| price | 34 | 8 | u64 (maker's resting price) |
| qty | 42 | 8 | u64 |
| resting_qty | 50 | 8 | u64 |
| state | 58 | 1 | u8 (0=PartiallyFilled, 1=Filled) |

`state` is computed once, from this fill's own `resting_qty` (`0` →
`Filled`, otherwise `PartiallyFilled`) — deliberately mirroring FIX's
`ExecutionReport`/`OrdStatus` (tag 39) design, a status *field* on one
execution message type, rather than adding a second `PartiallyFilled`
wire tag alongside `Filled` (which would be closer to FIX's `ExecType`,
tag 150, as a second discriminant). This protocol already collapsed
`ExecType` into the message tag itself (one tag per `core::Event`
variant, ITCH/OUCH-style) before this change; `state` recovers the
`OrdStatus` distinction the deliverable spec asks for without reopening
that collapse into a second tag.

**`Cancelled`** (tag 13, 33 bytes)

| field | offset | width | type |
|---|---|---|---|
| tag | 0 | 1 | u8 (=13) |
| stream_seq | 1 | 8 | u64 |
| engine_seq | 9 | 8 | u64 |
| account_id | 17 | 8 | u64 |
| order_id | 25 | 8 | u64 |

**`Replaced`** (tag 14, 42 bytes)

| field | offset | width | type |
|---|---|---|---|
| tag | 0 | 1 | u8 (=14) |
| stream_seq | 1 | 8 | u64 |
| engine_seq | 9 | 8 | u64 |
| account_id | 17 | 8 | u64 |
| order_id | 25 | 8 | u64 |
| new_qty | 33 | 8 | u64 |
| priority_retained | 41 | 1 | bool |

**`Trade`** (tag 15, 26 bytes) — market data, no account identity, no `engine_seq`

| field | offset | width | type |
|---|---|---|---|
| tag | 0 | 1 | u8 (=15) |
| stream_seq | 1 | 8 | u64 |
| price | 9 | 8 | u64 |
| qty | 17 | 8 | u64 |
| taker_side | 25 | 1 | u8 (0=Buy, 1=Sell) |

**`BookUpdate`** (tag 16, 43 bytes) — market data, top-of-book, no `engine_seq`

| field | offset | width | type |
|---|---|---|---|
| tag | 0 | 1 | u8 (=16) |
| stream_seq | 1 | 8 | u64 |
| bid_present | 9 | 1 | bool |
| bid_price | 10 | 8 | u64 (0 if bid_present=false) |
| bid_qty | 18 | 8 | u64 (0 if bid_present=false) |
| ask_present | 26 | 1 | bool |
| ask_price | 27 | 8 | u64 (0 if ask_present=false) |
| ask_qty | 35 | 8 | u64 (0 if ask_present=false) |

**`SnapshotLevel`** (tag 17, 34 bytes) — one per currently-occupied price level

| field | offset | width | type |
|---|---|---|---|
| tag | 0 | 1 | u8 (=17) |
| stream_seq | 1 | 8 | u64 |
| side | 9 | 1 | u8 (0=Buy, 1=Sell) |
| price | 10 | 8 | u64 |
| qty | 18 | 8 | u64 (level's total resting quantity) |
| order_count | 26 | 8 | u64 |

**`SnapshotAccount`** (tag 18, 41 bytes) — one per account with a resting order

| field | offset | width | type |
|---|---|---|---|
| tag | 0 | 1 | u8 (=18) |
| stream_seq | 1 | 8 | u64 |
| account_id | 9 | 8 | u64 |
| open_order_count | 17 | 8 | u64 |
| notional | 25 | 16 | u128 (gross, ticks — SPEC §5) |

**`SnapshotSummary`** (tag 19, 68 bytes) — trailer, one per `Snapshot` response

| field | offset | width | type |
|---|---|---|---|
| tag | 0 | 1 | u8 (=19) |
| stream_seq | 1 | 8 | u64 |
| bid_present | 9 | 1 | bool |
| bid_price | 10 | 8 | u64 (0 if bid_present=false) |
| bid_qty | 18 | 8 | u64 (0 if bid_present=false) |
| ask_present | 26 | 1 | bool |
| ask_price | 27 | 8 | u64 (0 if ask_present=false) |
| ask_qty | 35 | 8 | u64 (0 if ask_present=false) |
| last_trade_present | 43 | 1 | bool |
| last_trade_price | 44 | 8 | u64 (0 if last_trade_present=false) |
| level_count | 52 | 8 | u64 |
| account_count | 60 | 8 | u64 |

See "Snapshot" under Matching policies below for the response's meaning
and why the trailer comes last rather than first.

### Validation

All at the gateway, before risk or matching see anything: frame length
matches the tag's implied length, enum fields in range, quantity nonzero,
price nonzero except for `Market` (whose price is unused and wire value is
ignored). Malformed input rejects with a specific reason code and never
reaches `Engine::apply`.

---

## Running, benchmarking, replaying

### With no host Rust install (Docker)

A reviewer should never need to install Rust locally to run any of this.
`docker-compose.yml`'s `dev` service targets the `builder` stage of the
same `Dockerfile` directly — it has the full pinned toolchain (rustc
1.91.1, matching every number in [BENCH.md](BENCH.md) exactly) and the
whole workspace already built; `engine`'s own final stage
(`debian:bookworm-slim`) deliberately has no compiler at all, just the
release binary, so these run against `dev`, not `engine`:

```sh
docker compose up --build                                              # the daemon itself
docker compose run --rm dev ./check.sh                                 # fmt + clippy + full test suite
docker compose run --rm dev cargo bench -p core                        # criterion micro-benchmarks
docker compose run --rm dev cargo run -p bench --release -- --messages 1000000   # HDR burst workload
docker compose run --rm dev cargo run -p bin --bin replay -- recordings/sample.bin  # replay the shipped sample
```

All four `dev` commands above were run and confirmed clean in this
container (exit 0 on `check.sh`, both benchmark suites completing with
numbers consistent with the host run — see BENCH.md for a note on the one
real behavioral difference observed: sustained throughput matched the host
almost exactly, but latency was substantially lower, plausibly a smaller
effective socket buffer in Docker Desktop's Linux VM changing the queueing
profile at the same throughput ceiling, not a correctness difference).

### With a local Rust install

```sh
./check.sh                                    # fmt + clippy + full test suite
cargo bench -p core                           # criterion micro-benchmarks
cargo run -p bench --release -- --messages 1000000   # HDR burst workload (defaults: 200k msg/s, 70/20/10 mix)
cargo run -p bin --bin replay -- recordings/sample.bin  # replay the shipped sample recording
```

**Daemon**, standalone: `cargo run -p bin --bin engine` (optionally
`-- --record <path>` to capture a fresh recording). It binds
`/run/engine/order-entry.sock` and `/run/engine/market-data.sock` — on
Linux this directory needs to exist and be writable
(`docker compose up` handles this via a named volume automatically; running
the raw binary outside a container needs `/run/engine` created first). On
macOS, `/run` isn't a standard path at all — `docker compose up --build` is
the tested way to run the daemon on this development machine; running the
bare binary directly fails with `No such file or directory` unless that
directory is created first.

**Replay**: `cargo run -p bin --bin replay -- <recording-path>
[--risk-config <path>] [--exclude-timestamps]`. Writes the replayed
outbound stream's wire-encoded bytes to stdout, so two invocations can be
diffed externally (`diff`, `sha256sum`) as literal proof of byte-identical
replay, not just a claim. `recordings/sample.bin` ships in the repo,
produced by `cargo run -p bin --example generate_sample_recording`
(`crates/bin/examples/generate_sample_recording.rs`): two resting makers, a
full cross, a mass-cancel, a kill-switch-engaged reject, a fresh resting
order once the switch disengages, and a `Snapshot` of what's left resting
— including a risk rejection specifically, since that's the case
deterministic replay is most valuable for proving reproduces identically,
and a `Snapshot` specifically so the shipped sample exercises
`SnapshotLevel`/`SnapshotAccount`/`SnapshotSummary` too, not only the five
execution-report types.
`--exclude-timestamps` is accepted and documented but is currently a no-op
— see Known limitations.

---

## Matching policies

### Order types

| Type | Behaviour |
|---|---|
| `Limit { tif: Gtc }` | Match what it can, rest the remainder |
| `Limit { tif: Ioc }` | Match what it can, discard the remainder |
| `Limit { tif: Fok }` | Match in full immediately, or reject with zero fills |
| `Limit { tif: PostOnly }` | Reject if it would cross **after** self-trade prevention; otherwise rest in full |
| `Market` | IOC semantics with an unbounded limit; partial-fill-and-discard in a thin book, never rejected — `price = 0` on the wire, unused |

Price priority (best opposite price first), then strict time priority
(FIFO) within a level. Fills always execute at the resting (maker) order's
price, never the aggressor's limit. A locked or crossed book is
structurally impossible: price equality counts as crossing, so an order
that would lock the book matches instead of resting — asserted in
`assert_invariants()`, not merely assumed.

### Cancel and modify (`CancelReplace`)

Cancel is O(1), owner-checked. **No-oracle rule**: a cancel for an unknown
order id and a cancel for another account's order reject identically (same
reason code, same event shape) — you cannot use cancel to probe whether
someone else's order id exists.

| Change | Queue position |
|---|---|
| Quantity **decreased**, price unchanged | **Retained** |
| Quantity unchanged | **Retained** |
| Quantity **increased** | **Lost** — moves to the back of its level |
| Price **changed** | **Lost** — moves to the back of the new level |

Why the asymmetry: decreasing is riskless to whoever is queued behind
you — there's strictly less ahead of them. Increasing or repricing is a
new economic commitment, and letting it keep priority would turn amend into
a way to launder queue position. A price-changing modify that crosses
re-enters matching as a fresh aggressor, subject to STP exactly like a
fresh submit; `Replaced` (carrying the pre-match new quantity) is always
emitted before any `Filled` events that follow. Modify is never
implemented as cancel-then-submit — that would lose queue position in the
decrease case, defeating the point. A modify to zero quantity is rejected
(`ZeroQuantity`); zero is what `CancelOrder` is for.

### Self-trade prevention

Policy: **cancel-resting**. When an aggressor would match against a
resting order from the same account, the resting order is cancelled
(`Cancelled` emitted) and matching continues — the aggressor's quantity is
not consumed. Applies to submits and to a crossing modify.

Two alternatives were considered and rejected: cancel-aggressive (reject
the incoming order instead) and cancel-both. *The reasoning below reflects
my own judgment, not a documented spec decision.* Cancel-resting was
chosen because it preserves the most recent expression of the account's
intent: the resting order represents an older commitment, the incoming
aggressor represents what the account wants right now, and in a market
moving fast enough to trigger a self-cross at all, the more recent order
is the one that should win, not the stale one. Cancel-aggressive does the
opposite — it rejects the account's current, presumably better-informed
order in favor of an old resting one, which is backwards. Cancel-both
destroys real resting liquidity on every self-cross regardless of which
side is actually stale.

A real exchange would likely expose this as a configurable per-account or
per-order setting — several venues offer cancel-newest / cancel-oldest /
cancel-both / cancel-resting as an explicit choice — rather than one
hardcoded policy. Worth naming as something this build deliberately
doesn't attempt, not a gap discovered by accident.

**PostOnly's crossing check runs after self-trade prevention, not against
the raw book** — this is the one place the eager-vs-atomic distinction
matters and is worth stating precisely. If a PostOnly order would only
cross a resting order from the *same* account, STP cancels that resting
order for real, nothing remains to cross, and the PostOnly order rests —
it does not reject `WouldCross`. This was implemented **eagerly**: a
same-account resting order encountered while walking the crossing side is
cancelled for real, immediately, the moment the walk reaches it — not
merely excluded from a fillability count first. An **atomic, FOK-style
precheck** (determine whether foreign depth would be crossed *before*
cancelling anything) was considered and rejected, because it would break
the precedent already set by GTC, IOC, and Market: all three cancel a
same-account resting order for real, immediately, the instant their sweep
reaches it, regardless of what happens to the rest of the order
afterward. FOK is different because its precheck exists specifically to
guarantee an all-or-nothing fill — a genuine "nothing happened" contract.
PostOnly makes no such promise: whether it later rests or rejects for an
unrelated reason (foreign depth further down the book) doesn't
retroactively un-attempt a self-cross STP already prevented. Concretely: if
the walk finds a *foreign* resting order after already cancelling one or
more same-account orders, the PostOnly order rejects `WouldCross`, and
those earlier cancellations **stand** — they are not reversed.

### Mass-cancel

Cancels every resting order for one account, one `Cancelled` per order.
Emission order is the account's slot array's current traversal order —
deterministic (which is what byte-identical replay needs) but explicitly
**not** arrival order, since cancelling via `swap_remove` reorders the
array after any non-tail removal, and explicitly not `HashMap` iteration
order, which would break replay outright.

### Snapshot

An operator/inspection command, not order flow: `Book::snapshot` walks
the whole book read-only and dumps its current state to whichever
connection asked. `Engine::apply`'s own before/after top-of-book diff
naturally emits no `BookUpdate` for it, since a snapshot never mutates
anything.

The walk reuses `assert_invariants()`'s own traversal shape — the same
deterministic `bids` then `asks` `BTreeMap` order, then the same
`walk_level` helper `assert_invariants()` already uses to check each
level's chain — rather than a second, parallel walker. (`mass_cancel`
does not walk levels at all — it snapshots the target account's own
dense slot array and calls `unlink` once per order directly, which is
the whole point of the per-account index, SPEC §4; `walk_level` is
shared with `assert_invariants()` only.) For each occupied level it emits one
`SnapshotLevel` (side, price, total resting qty, order count) directly
from the level's own cached totals; while walking each level's orders it
also records which accounts it has seen, in first-seen order, and after
the whole book is walked it emits one `SnapshotAccount` per account
found (open order count and gross notional, both read directly from that
account's already-maintained `AccountEntry` — nothing here is
re-derived). `self.accounts` itself, a `HashMap`, is never iterated
directly for this — only a `Vec` built during the level walk supplies
output order, with a `HashSet` used solely for membership checking — so
no `HashMap` iteration order reaches the wire.

`SnapshotSummary` — best bid/ask, `last_trade`, and the level/account
counts — is sent **last, as a trailer, not a header**: by the time the
walk finishes, both counts are already known from the same pass that
produced the `SnapshotLevel`/`SnapshotAccount` messages, so sending the
summary last avoids either a second counting pass or buffering the whole
response up front just to learn a count. A reader also gets a clear
"the dump is complete" signal for free — the trailer arriving means
there is nothing more to come — without needing a separate end-of-stream
marker.

Snapshot's own per-call allocation (the `Vec`/`HashSet` above) is
accepted, not fixed, for reasons laid out in Known limitations below,
right next to `MassCancel`'s allocation fix. Using a snapshot as an
actual replay seed is out of scope — see Known limitations.

---

## Risk controls

Checked on the matching thread, before `Engine::apply`, on every command.
Configuration lives in `risk.toml` (or `risk-bench.toml` for the benchmark
harness — see [BENCH.md](BENCH.md)), loaded with built-in defaults so
`docker compose up` needs no setup:

```toml
max_open_orders = 50            # per account
max_notional_ticks = 100_000_000  # $1,000,000 at 1 tick = $0.01
price_band_pct = 10             # ±10%
```

### Kill switch — drain policy

When engaged, new order entry is rejected (`KillSwitchActive`); resting
orders stay live, matchable, and cancellable until cleared explicitly. A
venue that unilaterally cancels every resting order on halt strands
counterparty exposure — draining is the safer default. Checked *inside the
matching loop*, not only at ingress, so a command already queued when the
switch fires is still rejected against the new state.

`CancelReplace` during drain: **blocked** if it increases quantity or
changes price (a new economic commitment, exactly like `NewOrder`);
**allowed** if it only decreases quantity, for the same reason a decrease
keeps queue priority elsewhere — it strictly reduces exposure. `CancelOrder`
(pure removal) is always allowed during drain.

### Per-account limits

`max_open_orders` and `max_notional` (gross: `price × qty` summed across
*all* resting orders regardless of side, plus the incoming order, read
O(1) from `AccountEntry`). A two-sided quote of 500k on each side consumes
the full 1M cap. Stored as `u128` so the multiplication can't silently wrap
before validation catches malformed input.

Market orders carry `price = 0` on the wire, which would read as zero
notional regardless of size if used directly — the reference price for a
Market order's notional check is resolved separately: last trade price if
any trade has occurred, else the best price on the side it would sweep,
else (nothing to fill against at all) rejected `NotFullyFillable` — not a
new rejection, the same "nothing to fill" outcome restated.

### Price band

Rejects `PriceBandViolation` outside ±10% of a reference price: last trade
if any, else the book mid if both sides have depth, else the check is
skipped (an empty or one-sided book has no defensible reference — inventing
one from a single side would derive the band from the very orders it's
meant to constrain). Does not apply to `Market` orders, since a Market
order can only fill against already-resting depth that already passed the
band on entry.

### Reason codes

Exhaustive `RejectReason` enum, one wire byte value each:

| Reason | Wire value |
|---|---|
| `MalformedMessage` | 0 |
| `UnknownMessageType` | 1 |
| `DuplicateOrderId` | 2 |
| `UnknownOrderId` | 3 |
| `ZeroQuantity` | 4 |
| `WouldCross` | 5 |
| `NotFullyFillable` | 6 |
| `KillSwitchActive` | 7 |
| `MaxOpenOrders` | 8 |
| `MaxNotional` | 9 |
| `PriceBandViolation` | 10 |
| `ZeroPrice` | 11 |

`ZeroPrice` is distinct from `ZeroQuantity`: a zero-price non-`Market`
order is a specific, nameable violation ("price nonzero except for
Market"), not a generic `MalformedMessage`.

---

## Units

- **Tick size: $0.01.** `Price` is a `u64` count of ticks.
- **Lot size: 0.0001 base units.** `Qty` is a `u64` count of lots.
- `price × qty` is therefore denominated in quote-currency cents, which is
  exactly what the notional check sums and caps — for a BTC/USDT book,
  price is USDT-per-BTC in cents, qty is BTC in ten-thousandths, and the
  product is USDT cents.

---

## Assumptions and tradeoffs

Every one of these is a rejected alternative from SPEC.md's own design
record, not a hypothetical:

**Dense per-account slot array vs. an intrusive account chain.** A second
intrusive doubly-linked chain through the same nodes (mirroring the
per-level chain) was considered and rejected on constants, not asymptotics:
it costs +16 bytes on `Node` (pushing it past the 64-byte cache-line
budget), two random writes per cancel instead of at most one, a random
write on every rest, and a second parallel structure to keep consistent.
The dense slot array with a back-index (`Node::acct_idx`) is O(1) on the
same operations with strictly smaller constants.

**`BTreeMap<Price, Level>` vs. a sorted `Vec` vs. a flat tick-indexed
array.** A pre-reserved sorted `Vec<(Price, Level)>` with binary search
would make best-bid/ask O(1) (first/last element) instead of a tree
traversal, and insert/remove would be an O(n) `memmove` over contiguous
memory — at realistic level counts (tens to low hundreds), likely cheaper
in practice than tree pointer-chasing, and `reserve` makes it provably
non-allocating. Not adopted: replacing the book's core index late in the
build carries more risk than the measured gain justifies, and `BTreeMap`'s
allocation behavior (§Book data structures rationale) is already within
target. A flat array indexed by tick over a band around the touch, with
`BTreeMap` fallback outside it, would remove the allocation and the
O(log n) lookup together — and is what a latency-critical production
implementation would use — but needs band sizing, a re-centring policy,
and a fallback path: more machinery than remaining scope supported. Named
with its cost rather than shipped half-tuned.

**UDS vs. TCP.** Covered under Architecture above.

**Gross vs. net notional.** Net (bid minus ask) was considered and
rejected: it doesn't bound worst-case exposure (nothing stops both sides
of a two-sided quote filling in quick succession during a fast move, real
turnover the account incurred while appearing flat), and it permits
unbounded gross book presence while net-flat — a market-integrity problem
independent of risk. Gross is also the cheaper implementation: one
unsigned running total, no sign tracking.

**Eager vs. atomic self-trade prevention for PostOnly.** Covered under
Matching policies above.

---

## Known limitations

- **`recv_ts` is not implemented.** SPEC §3 describes the gateway stamping
  a receive timestamp on arrival; no field carries it on `Command` today.
  `--exclude-timestamps` (SPEC §7) ships as a documented no-op until it
  does — deliberately deferred rather than added as a mechanical side
  effect of the determinism stage, since it was never in that stage's
  declared scope.
- **Signal-based graceful shutdown (SIGINT/SIGTERM) is not wired up.**
  Tolerated by the existing stale-socket-unlink-on-bind behavior: an
  unclean stop leaves a socket file behind, and the next bind removes it.
- **An isolated marketdata-only backpressure test was attempted, found
  flaky under sustained load for an undiagnosed reason, and dropped** in
  favor of equivalent coverage proven end-to-end in
  `crates/bin/tests/market_data.rs` (fast/slow subscriber isolation,
  execution-report/market-data `StreamSeq` independence, and
  order-entry/matching progress under a stalled subscriber).
- **`Book::accounts` under continuous account churn is unmeasured.** Its
  `HashMap` only ever grows (no account entry is ever removed); if the
  account population churns continuously through a session rather than
  stabilizing after startup, an occasional bucket-array rehash carries the
  same allocation category `MassCancel`'s clone did before it was fixed.
  See [BENCH.md](BENCH.md) for the concrete next step
  (`ACCOUNTS_INITIAL_CAPACITY`, mirroring `gateway`'s
  `RESTING_CONN_INITIAL_CAPACITY`).
- **`Snapshot`'s allocation is accepted, not fixed** — a deliberate line,
  not an oversight, worth stating next to `MassCancel`'s fix above so the
  asymmetry reads as a choice rather than an inconsistency. `MassCancel`
  is client-invoked order flow and belongs to the hot path, so its
  per-call `Vec` clone was a real bug, fixed with a reused scratch buffer.
  `Book::snapshot` also allocates once per call (a `Vec`/`HashSet` used to
  track which accounts the traversal has seen), and that allocation is
  *not* fixed the same way, because `Snapshot` is an operator-invoked
  admin/debug/inspection command, not client-invoked order flow — it runs
  on the order of once per incident, not once per message, so holding it
  to the same zero-allocation standard would be optimizing something that
  was never on the hot path to begin with. What makes something "hot
  path" here is whether it's on the per-order flow, not whether it
  happens to run on the matching thread. The allocation is bounded by the
  size of the book at the moment `Snapshot` is called, and
  `hot_path_allocates_nothing` (`crates/risk/tests/zero_alloc.rs`)
  deliberately excludes `Snapshot` from its measured command sequence for
  exactly this reason. Using a snapshot as an actual replay seed (loading
  it to start replay from mid-session state, rather than only reading it)
  is separately out of scope — this build implements the inspection half
  only, not the seed-a-replay-from-here half.

**Only the matching thread does either of these today, and only one of
the two (busy-spinning; SPEC §4) — the gateway, market-data, and
return-dispatcher threads all still block on their reads/channel
receives, and no thread, including the matching thread, is pinned to a
dedicated core yet (stage 9, SPEC §10, Extension slice, not yet built as
of this writing).** The two mechanisms aren't equal contributors to the
latency number. Busy-spinning does the heavy lifting: it eliminates the
wake-up latency of blocking on a read or channel receive — the kernel
marking a thread not-runnable, waiting for a wake signal, then
rescheduling it — which is the dominant cost in the four-thread-wake-up
chain BENCH.md's Docker reproduction section describes (matching logic
itself is roughly 1% of the measured per-message time; the rest is socket
syscalls and OS scheduling). Pinning's role is narrower: it mainly
prevents the OS scheduler from migrating a spinning thread mid-run
(losing its warm cache state) or preempting it to run something else —
either of which would reintroduce the jitter spinning is meant to remove.
Pinning is closer to a precondition for a *clean* spin than an equal
contributor to the latency number by itself.

A real low-latency deployment would both spin and pin every thread in the
hot path, not just one. That isn't done here because dedicating
four-plus cores to one symbol's gateway threads doesn't scale past a
handful of symbols, and because the real fix for socket-syscall overhead
is kernel-bypass (DPDK, already described as future work against the
`Transport` trait in the Architecture section above), not spinning more
threads around the same syscalls. Full pin-and-spin plus kernel-bypass
together is what a production low-latency venue would actually run;
stage 9's plan is to pin and spin the one thread where it matters most
for the specific requirement (SPEC §9's zero-or-near-zero-allocation,
low-jitter hot path), not to apply it uniformly across all four.

---

## Microstructure: why a CLOB, and where it stops being the right answer

This system's matching rules — price priority, then strict FIFO, maker-price
execution, self-trade prevention by cancelling the resting side — are the
standard continuous-limit-order-book contract, chosen because they're the
predictable, well-understood baseline every participant already reasons
about, and because they're what makes byte-identical deterministic replay
tractable: a CLOB's state transition is a pure function of the ordered
command stream, with no auction-clearing computation or batch-window
logic in between.

**Where this shape fits a crypto-native, ZK-settled venue.** The apparent
mismatch is speed: a CLOB matches in microseconds, and ZK proof generation
does not. The resolution is architectural, not a compromise on either
side — use this as the *matching layer* of a hybrid system, with
settlement batched separately on the ZK layer. Matching stays fast and
continuous; proof generation runs against accumulated batches rather than
per-trade. The CLOB's speed is then spent doing what it's good at —
reducing time-to-fill for the participant — rather than being gated by
proof latency it was never going to be fast enough to hide.

**Where this shape stops fitting: as the primary venue in an intent-based
system.** An intent-based platform's core mechanic is solvers competing to
fill user intents, and that competition is structurally an RFQ or auction
pattern, not an order-book pattern. A limit order has a natural "rest in a
queue until matched" existence; an intent does not — it's a one-shot
request answered by competing solvers, not a standing commitment with a
queue position to protect or lose. Pointing users directly at a CLOB like
this one in that context is using the wrong primitive for the interaction
they're actually having. The honest framing: this is a good *liquidity
source* a solver taps on a user's behalf, not a good venue for a user to
interact with directly, in an intent-based design specifically.

**Where frequent batch auctions win outright: MEV protection.** This
system's price-time priority is directly exploitable by whoever wins the
race to be first in the queue — and nothing in the current design defends
against that; it is not resistant to front-running or latency arbitrage in
any way. A frequent batch auction with uniform-price clearing over a
window removes the incentive to win an ordering race *within* that
window, since everyone in the batch clears at the same price regardless of
submission order inside it. Where MEV resistance matters more than
sub-millisecond price discovery, that property beats this design's speed
advantage outright. This is a genuine, honest gap in what's built here —
not a footnote to caveat around.

**The headline limitation of this specific build, and its evolution
path: single-symbol, and UDS's single-host ceiling.** One book, one
process, one machine. Multi-symbol doesn't need a new matching design — it
needs sharding, one matching thread (one writer) per shard, which is
exactly this book's own single-writer discipline applied again one level
up: no new concurrency model, just more of the one already proven here.
Multi-host needs replacing UDS with a real network transport for order
entry — TCP or FIX, sitting behind the `Transport` trait that already
exists for exactly this reason — and multicast for market-data fan-out,
since unicast to N remote subscribers doesn't scale the way local
broadcast does. Neither is a redesign; both are the abstraction boundary
already in place being used for what it was built for.
