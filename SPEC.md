# Matching Engine — Specification

Single-symbol central limit order book with exchange-style connectivity,
deterministic replay, tail-latency measurement, and risk controls.

This document is the contract. If an instruction here conflicts with something
said in conversation, this document wins unless explicitly amended. Amendments
are made in this file and committed, so the decision history stays visible.

**Scope note.** §11 divides the build into a Core slice, scoped to roughly three
hours of implementation, and an Extension slice built deliberately beyond it.
Design, architecture, and this specification account for roughly two hours ahead
of the first line of code. Each extension carries its rationale. The division was
made before any code existed.

---

## 1. Non-goals

Stated first, deliberately. Out of scope, not attempted:

- Persistence, settlement, crash recovery
- Multiple symbols (one book; sharding discussed in the README, not built)
- Remote or cross-host connectivity (see §3 — deliberate, with rationale)
- L2 incremental market data with snapshot+delta recovery (top-of-book only)
- Frequent batch auction mode
- Conformance fuzzer; Prometheus/OpenMetrics counters
- Hugepages and NUMA-aware placement (described in README, not implemented — §10)
- DPDK / kernel-bypass ingress (described; the transport trait exists to
  accommodate it — §3)

---

## 2. Domain model

### Units

- `Price`: `u64` integer ticks of **quote currency per base-currency lot**.
  **1 tick = $0.01.** Never a float.
- `Qty`: `u64` integer lots of **base currency**. **1 lot = 0.0001 base unit.**
- Therefore `price × qty` is denominated in quote currency, which is what makes
  the notional arithmetic in §5 mean what it says. For a BTC/USDT book: price is
  USDT-per-BTC in cents, qty is BTC in ten-thousandths, product is USDT cents.
- `OrderId`: `u64`, client-assigned, unique among an account's *currently
  active* orders. **Not globally unique** — two different accounts may submit
  the same numeric id concurrently. Every lookup, index, and duplicate check is
  therefore keyed on `(AccountId, OrderId)`, never bare `OrderId` (§4).
- `AccountId`: `u64`. Every order carries one. Not optional.
- `EngineSeq`: `u64`, engine-assigned, strictly monotonic across every command
  the engine applies. Used for internal ordering and the determinism
  comparison in §7. Not a per-stream sequence number — see `StreamSeq` below.
- `StreamSeq`: `u64`, one independent monotonic counter **per outbound
  stream** — one for execution reports, one for market data. Gap detection
  (§8) is only meaningful against a counter that increments exactly once per
  message actually delivered on that stream. A single counter shared across
  streams increments on every event regardless of which stream carries it, so
  a subscriber to one stream sees permanent phantom gaps for every event that
  went out the other stream. Each outbound message carries its stream's
  `StreamSeq`, not `EngineSeq`.
- Symbol is hardcoded to one instrument. Tick size, lot size, and all risk
  defaults are named constants.

### Order types

| Type | Behaviour |
|---|---|
| `Limit { tif: Gtc }` | Match what it can, rest the remainder |
| `Limit { tif: Ioc }` | Match what it can, discard the remainder |
| `Limit { tif: Fok }` | Match in full immediately, or reject with zero fills |
| `Limit { tif: PostOnly }` | Reject if it would cross **after** self-trade prevention; otherwise rest in full |
| `Market` | IOC semantics with an unbounded limit |

Market order in a thin book: **partial-fill-and-discard**, never reject. Fills
whatever depth exists, discards the remainder, emits `Accepted` with
`resting_qty: 0`. The README documents this as a deliberate choice against the
reject alternative.

Market orders carry `price = 0` on the wire. The field is present because the
message layout is fixed-width and uniform across order types, but it is
unused — the matching sweep substitutes an unbounded bound (`Price::MAX` for a
buy, `0` for a sell) as its stop condition. Gateway validation therefore exempts
`Market` from the nonzero-price rule (§3), and the price band does not apply
(§5).

### Matching rules

1. **Price priority.** An aggressor consumes the best opposite price first,
   sweeping levels while its limit permits.
2. **Time priority.** Strict FIFO within a price level.
3. **Maker price execution.** Fills occur at the resting order's price.
4. A resting order's queue position derives from when it *rested*, not when it
   was first submitted.

**Locked and crossed books are structurally impossible.** The crossing predicate
treats price equality as crossing (`level_price <= limit_price` for a buy), so
an order that would lock the book matches instead of resting. Asserted in
`assert_invariants()`, not merely assumed.

### Cancel

Removes a resting order. O(1). Requires the requesting `AccountId` to match the
resting order's owner.

**No-oracle rule:** a cancel for an unknown id and a cancel for another
account's order reject **identically** — same reason code, same event shape — so
cancel cannot be used to probe whether another account's order id exists.

### Modify (CancelReplace)

| Change | Queue position |
|---|---|
| Quantity **decreased**, price unchanged | **Retained** |
| Quantity unchanged | **Retained** |
| Quantity **increased** | **Lost** — moves to back of its level |
| Price **changed** | **Lost** — moves to back of the new level |

Rationale for the asymmetry (for the README): decreasing is riskless to orders
queued behind you — there is strictly less ahead of them. Increasing or
repricing is a new economic commitment, and allowing it to retain priority would
make amend a mechanism for laundering queue position.

A price-changing modify that crosses re-enters matching as a fresh aggressor at
the new price, subject to STP identically to a fresh submit. Event ordering:
`Replaced` (carrying pre-match `new_qty`, `priority_retained: false`) is emitted
**before** the resulting `Filled` events.

A modify to `new_qty == 0` is rejected with `ZeroQuantity`. A modify to zero is a
cancel, and clients send `CancelOrder` for that; accepting it here would create a
second path to the same outcome with different event shapes, for no gain. This
matches submit validation, which also rejects zero quantity.

Modify must **not** be implemented as cancel-then-submit — that loses queue
position in the decrease case.

Ownership is checked identically to cancel, including the no-oracle rule.

### MassCancel

Cancels every resting order for one account, emitting one `Cancelled` per order.

**Emission order is the per-account slot array's traversal order** (§4). This is
deterministic, which is what byte-identical replay requires, and that is the only
property depended upon. It is explicitly **not** arrival order: the slot array
uses `swap_remove` on cancel, which moves the last element into the vacated
position, so after any non-tail cancel the array no longer reflects insertion
sequence. It is also explicitly not `HashMap` iteration order, which would break
replay outright.

Ordering by `OrderId` was considered and rejected: nothing in the system depends
on mass-cancel ordering beyond determinism, and imposing it would cost a sort on
an operation that has no reason to pay for one.

### PostOnly and self-trade prevention ordering

PostOnly's crossing check is evaluated **after** STP would have applied, not
against the raw book. Concretely: if a PostOnly order would only cross a
resting order belonging to the *same* account, STP cancels that resting order
first, nothing remains to cross, and the PostOnly order rests. It does not
reject `WouldCross`.

This is **not** FOK's precheck, and must not reuse it. FOK's `is_fillable`
traversal is deliberately read-only — it exists to buy certainty *before*
mutating (§6), because FOK needs to know whether it can fill in full before
committing to anything. PostOnly has no such constraint: cancelling a
same-account resting order is a decision it can commit to immediately, there
is no partial-fill state to protect. Reusing the read-only traversal for
PostOnly would leave the same-account resting order untouched whenever the
only crossing depth was self-owned — both orders would then rest at crossing
prices, violating "locked and crossed books are structurally impossible" and
the `assert_invariants()` check that best bid strictly < best ask always
holds, with no same-account exception.

**Considered and rejected: an atomic, FOK-style precheck** that determines
whether the order would end up crossing foreign depth *before* cancelling
anything, only mutating once the outcome is known. Eager — cancelling a
same-account resting order for real the instant the sweep reaches it — was
chosen instead, because it matches the precedent already set by GTC, IOC,
and Market: all three cancel a same-account resting order for real,
immediately, the moment their sweep reaches it, regardless of what happens
to the rest of the order afterward. PostOnly and FOK are the two order
types with a genuine "reject the whole thing" outcome, which raises the
question of whether a reject should mean "nothing happened, including to
unrelated resting orders." FOK answers yes, because its precheck exists
specifically to guarantee an all-or-nothing fill. PostOnly answers no:
STP's job is to prevent a self-cross the moment the sweep attempts one, and
whether the submitted order later rests or rejects for an unrelated reason
doesn't retroactively un-attempt that self-cross.

Implemented as: PostOnly's check walks the crossing levels on the opposite
side, in the same price-time order a real sweep would. A same-account resting
order encountered during the walk is cancelled for real via the ordinary
cancel-resting path — same unlink, same `Cancelled` event — not merely
excluded from a count. The walk continues past it. A foreign resting order
encountered during the walk stops the check immediately: the PostOnly order
rejects `WouldCross`, and any same-account cancellations already performed
earlier in the walk stand — not reversed, per the decision above. If the
walk reaches the end of crossing depth without finding any foreign order,
the PostOnly order rests in full. No foreign quantity is ever matched or
consumed by a PostOnly order under any outcome.

### Self-trade prevention

Policy: **cancel-resting.** When an aggressor would match a resting order with
the same `AccountId`, the resting order is cancelled (emitting `Cancelled`) and
matching continues; the aggressor's quantity is not consumed. Applies to submit
aggressors and to a crossing modify.

Alternatives considered and rejected — document both in the README:
cancel-aggressive, cancel-both.

---

## 3. Transport and wire protocol

### Transport

**Unix Domain Socket, `SOCK_STREAM`.** Two paths:

- `/run/engine/order-entry.sock` — order entry, request/response per connection
- `/run/engine/market-data.sock` — market data, broadcast to N subscribers

Rationale for the README: the system is scoped to a single process on a single
host. UDS removes the entire TCP/IP stack from the measurement path, so
benchmark numbers describe the engine rather than the loopback stack. It also
provides kernel-enforced access control via filesystem permissions and
kernel-verified peer identity via `SO_PEERCRED`, which matters for the control
plane.

Honest framing for the README: a production venue uses TCP (FIX or a binary
equivalent) for external order entry, since clients are remote; UDP multicast for
market-data fan-out, since unicast does not scale to many subscribers; and often
shared-memory ring buffers for the internal gateway↔matcher hop. UDS is the
correct choice under the constraints here.

**Transport trait.** Ingress sits behind a trait, so transport is a compile-time
choice rather than an architectural commitment. A DPDK implementation would be a
second `impl` of the same trait, with all `unsafe` FFI confined to that module
behind an RAII mbuf wrapper. Nothing in `core`, `wire`, or `risk` changes.

Stale socket files are unlinked on bind and cleaned up on shutdown.

### Framing

SBE-inspired, fixed-layout, **little-endian** (native on x86-64 and ARM64;
stated explicitly in the README).

- Byte 0: message type tag (`u8`)
- Bytes 1..N: fixed-width fields in a documented order

The tag determines total message length, so no separate length prefix is needed.
Because `SOCK_STREAM` provides a byte stream with no message boundaries, the
gateway buffers reads and extracts as many complete messages as are present,
retaining any partial trailing message for the next read. A single `read()` may
return a partial message, several messages, or both.

Every message type gets a layout table in the README: field name, offset, width,
type.

### Inbound messages

`NewOrder`, `CancelOrder`, `CancelReplace`, `MassCancel`, plus the control
plane: `KillSwitch`, `Snapshot`.

Orders carry: `order_id`, `account_id`, `side`, `price`, `qty`, `order_type`,
`tif`, `client_ts`. The gateway stamps `recv_ts` on arrival.

### Outbound messages

**Execution reports** carry: `order_id`, `stream_seq` (this stream's counter,
per §2), state (`accepted` / `rejected` / `partially_filled` / `filled` /
`cancelled`), fill `price` and `qty`, and a `reason` code on reject.

**Market data**: `Trade` prints on every execution; `BookUpdate` (top-of-book)
on every book change. Both carry monotonic sequence numbers so a subscriber can
detect gaps.

### Validation

All validation happens at the gateway, before risk or matching see anything:
frame length matches the tag's implied length, enum fields in range, quantity
nonzero, and price nonzero **except for `Market`**, whose price field is unused
and carries `0` (§2). Malformed input rejects with a specific reason code and
never reaches `Engine::apply`.

---

## 4. Architecture

Workspace crates, dependencies pointing downward only:

- **`core`** — `Engine`, `Book`, `Arena`, `Level`, `AccountEntry`, domain types,
  matching logic. **Zero dependencies.** Knows nothing about sockets, threads,
  or bytes.
- **`wire`** — message schemas, encode/decode, framing. Depends on `core`, and
  decodes directly into `core` types (no parallel type hierarchy, no conversion
  layer).
- **`risk`** — kill switch, per-account limits, price band. Depends on `core`.
- **`gateway`** — `Transport` trait, UDS implementation, connection management,
  validation. Depends on `wire`, `risk`, `core`.
- **`marketdata`** — top-of-book and trade publishing. Depends on `wire`, `core`.
- **`bin`** — the daemon; wires everything together.
- **`bench`** — benchmark client; a real client of the same socket.

### Thread topology

Three threads. Each I/O thread owns its own sockets; no socket is touched by more
than one thread.

**Gateway thread** owns the order-entry socket. Accepts connections, assigns a
`ConnId`, reads and frames bytes, validates, and pushes `(ConnId, Command)` onto
a bounded channel. It also owns the write side for that socket: it receives
`(ConnId, Event)` items back, one per event, and writes each execution report
to the originating connection as it arrives — never a `Vec<Event>`, per the
channel design below.

**Matching thread** is the sole writer to `Engine`. It **busy-spins** on the
command channel rather than blocking, avoiding futex wake and context-switch
jitter in the tail. Per iteration: check kill switch, check risk, call
`Engine::apply`, dispatch events to the gateway return channel and the market
data channel. Pinned to a dedicated core (§10).

**Market data thread** owns the market-data socket and its subscribers. It
drains a bounded channel fed by the matching thread and writes to each
subscriber.

Market data gets its own thread rather than sharing the gateway's for a specific
reason: a slow subscriber blocking on `write()` must not stall order-entry
reads. Folding both into one thread would couple market-data backpressure to
order ingress, which is exactly the coupling the drop-oldest policy in §8 exists
to prevent. Separating them means a slow subscriber degrades only its own feed.

`ConnId` is passed through opaquely — `Engine` never sees it. This is what keeps
`core` free of any connection concept.

**Channels carry individual `(ConnId, Event)` items, never a `Vec<Event>`.**
`Event` is small and `Copy`. A channel of `Vec<Event>` looks bounded and
pre-allocated at the channel level, but the payload is a fresh heap allocation
on every dispatch that produces one or more events — which is every submit,
cancel, and modify, i.e. the entire hot path. `std::mem::take` on a reused
output buffer does not avoid this either: it replaces the buffer with a
zero-capacity `Vec`, so the next call's first push reallocates. One send per
event, not one send per command, is what actually satisfies the zero-allocation
rule in CLAUDE.md. The matching thread sends `Filled` and `Accepted`/`Rejected`
individually as they are produced, not batched into a collection first.

### Book data structures

- `BTreeMap<Price, Level>` per side; best bid is the last key, best ask the
  first. Empty levels are removed eagerly so both ends are always live prices.
- `Level`: intrusive doubly-linked FIFO threaded through an arena, plus cached
  `total_qty` and `count` so depth queries and the FOK precheck don't walk the
  chain.
- `Arena`: slot-stable `Vec<Option<Node>>` with a free list. Slot indices stay
  valid until freed, which is what allows plain `u32` links instead of
  `Rc<RefCell<_>>` — no per-node allocation, no runtime borrow checks, no
  reference-cycle leak risk.
- Order index: `HashMap<(AccountId, OrderId), u32>` mapping id to arena slot.
  Keyed on the pair, not bare `OrderId` — §2 scopes id uniqueness *per account*,
  so two different accounts may legally submit the same numeric id, and a bare
  `OrderId` key cannot hold both without one silently overwriting the other's
  index entry. The slot itself carries side, price, and account, so cancel and
  modify key straight into the right level.

  Keying on the pair also removes a separate ownership-comparison step: a
  lookup with the wrong `AccountId` simply misses, identical in shape to an
  unknown id. The no-oracle rule (§2) falls out of the index design rather than
  needing an explicit check after a successful lookup.

Rationale for the README, with the rejected alternative: an index lookup alone
only locates the level; without intrusive links, removal from within a level is
O(n). A `VecDeque` with tombstones gives amortised O(1) cancel but leaks level
memory until swept and lets cached aggregates drift. Cancel-heavy flow is the
common case, so the arena wins.

**Allocation behaviour of the price map.** Operations against an existing price
level never allocate. Creating or removing a level *may* allocate, but far less
often than once per level:

Rust's `BTreeMap` uses B=6, so each node holds up to 11 key-value pairs. An
insert allocates only when a node is full and must **split**; otherwise the key
occupies existing slack. Removal deallocates only when a node falls below
minimum occupancy and **merges**. So the cost is amortised across roughly an
order of magnitude more level creations than allocations.

More importantly, it converges toward zero in steady state. An active book's
levels churn *within a band around the touch* — prices empty and repopulate, but
the set of distinct prices in play stays roughly constant. The tree's node
structure therefore stabilises: inserts land in slack, removals stay above the
merge threshold, and restructuring stops. **Level churn is frequent; node churn
is not.**

This is measured rather than asserted. §6 splits the allocation test in two, and
§9 reports the churn figure as **allocations per 1,000 hot-path operations**
under the burst workload — directly comparable against a near-zero target,
rather than a per-level figure that would overstate the rate.

**Rejected alternatives**, both documented in the README:

*Pre-reserved sorted `Vec<(Price, Level)>` with binary search.* At realistic
level counts (tens to low hundreds) this is arguably better on every axis:
`reserve` makes it provably non-allocating, best bid and ask become O(1) as the
first and last elements rather than a tree traversal, and lookup is a binary
search over contiguous memory instead of pointer-chasing between nodes. Insert
and remove are O(n) `memmove`, but n is small and a `memmove` over a few
kilobytes of contiguous memory is vectorised, prefetch-friendly, and branch-free
— cheaper in practice than the tree traversal it replaces. This is the same
reasoning that selected the dense per-account slot array over an intrusive
chain: at small n, contiguity beats pointer-chasing and constants beat
asymptotics. It was not adopted because replacing the book's core index
structure late in the build carries more risk than the measured gain justifies,
and the allocation behaviour above is already within target.

*Flat array indexed by tick over a band around the touch, with `BTreeMap`
fallback outside the band.* Removes the allocation and the O(log n) lookup
together, and is what a latency-critical production implementation would use. It
needs band sizing, a re-centring policy as the touch moves, and a fallback path
— more machinery than the remaining scope supports. Naming it with its cost is
more useful than shipping a half-tuned version.

**`Node` is intended to stay within one 64-byte cache line.** Matching sweeps
touch nodes sequentially, so node size dominates far more than any single
auxiliary structure. Adding a field to `Node` requires a deliberate size check.

### Per-account index

Per-account state must answer two questions in O(1), without walking the book:
current open-order count and notional (needed on **every** submit, for risk),
and the set of an account's resting orders (needed for MassCancel).

```
AccountEntry {
    slots: Vec<u32>,   // arena slots, reserved to max_open_orders at creation
    notional: u128,    // sum of price × qty across resting orders
}
accounts: HashMap<AccountId, AccountEntry>
```

`Node` carries `acct_idx: u32` — its own position in its account's `slots`
array. Maintenance:

- **rest**: `slots.push(slot)`; set `node.acct_idx`. Contiguous write, no random
  access.
- **cancel / unlink**: `slots.swap_remove(node.acct_idx)`; if an element moved
  into that position, write its node's `acct_idx`. **At most one** random write.
- **MassCancel**: snapshot `slots` in its current order, then call the same
  cancel/unlink path once per order in that snapshot. Each call's
  `swap_remove` acts on the live array as it stands at that moment, which is
  correct regardless of how many earlier iterations in the same mass-cancel
  already shrank it. This costs `N` real `swap_remove`s rather than a single
  bulk `clear()` at the end — deliberately: account-index maintenance lives
  in exactly one path, `unlink`, never a second one bolted on for the bulk
  case, which is what CLAUDE.md's "exactly two places — `rest` and `unlink`"
  rule is protecting. The bulk-`clear()` shortcut was considered and
  rejected on those grounds.
- **Risk check**: `slots.len()` and `notional`, both O(1) behind one map lookup.

Open-order count is `slots.len()` rather than a separate cached field, so there
is one fewer value that can drift out of sync.

`slots` is reserved to `max_open_orders` when the account is first seen and
never grows, because risk rejects at the cap before it could. That makes it a
one-time allocation per account, not a hot-path allocation.

Rationale for the README, with the rejected alternative: a second intrusive
doubly-linked chain threaded through the same nodes (an account chain alongside
the level chain) was considered. It was rejected on constants, not asymptotics —
it costs +16 bytes of `Node` (pushing past the cache line), two random writes per
cancel instead of at most one, and a random write on every rest; it also adds a
second parallel structure to keep consistent. The dense slot array with a
back-index is O(1) on all the same operations with strictly smaller constants and
one fewer invariant to maintain.

---

## 5. Risk and operational controls

Checked on the matching thread, **before** `Engine::apply`, on every command.

### Kill switch

Policy: **drain.** When engaged, new order entry is rejected
(`KillSwitchActive`). Resting orders stay live — they remain matchable and
cancellable — until the operator clears the book explicitly.

Rationale for the README: a venue halting inbound flow while unilaterally
cancelling every resting order strands counterparty exposure and is its own
incident. Draining is the safer default. The alternative (reject-outstanding) is
simpler to verify but more aggressive.

Observed **inside the matching loop**, not only at ingress — an order already in
the channel when the switch is thrown must still be rejected.

`CancelReplace` during drain is **blocked**, treated as an amend rather than
cancel-adjacent — consistent with §2's own modify rationale, which treats any
quantity increase or price change as a new economic commitment. Drain halts new
commitments; a `Modify` that increases size or reprices is one, even against an
order that predates the drain. `Cancel` (pure removal) remains allowed, and a
`Modify` that only decreases quantity is allowed for the same reason a decrease
retains priority elsewhere in §2 — it strictly reduces exposure, never adds to
it.

### Per-account limits

Configured in `risk.toml`, with defaults baked in so `docker compose up` needs
no setup:

- `max_open_orders`: **50** per account
- `max_notional`: **100_000_000 ticks** ($1,000,000)

**Market orders carry `price = 0` on the wire (§2), which the notional formula
below would otherwise read as zero notional regardless of size — an unbounded
market order is the more dangerous case, not an exempt one.**

The reference price for a Market order's notional is resolved differently from
the price band's, because the two checks depend on different things. The band
needs a mid, which needs both sides. A market order only needs depth on the
side it sweeps — §2's "partial-fill-and-discard, never reject" rule is
unconditional on exactly that basis, and the notional check must not contradict
it.

Resolution, in order:

1. Last trade price, if any trade has occurred.
2. Otherwise, the best price on the side the order would sweep (best ask for a
   buy, best bid for a sell) — this exists whenever the order has anything to
   fill against at all.
3. Otherwise there is no depth on the relevant side and nothing to fill, and
   the order is rejected `NotFullyFillable` — the same outcome §2 already
   describes for an empty book on that side, not a new rejection this check
   invents.

Case 3 is not a notional-specific rejection — it is the pre-existing "nothing to
fill against" case restated. A Market order with real depth to sweep is never
rejected for notional reasons before it has a chance to fill; it is capped
against the price it will actually trade near, not blocked pre-emptively.

Notional is **gross**: `price × qty` summed across *all* the account's resting
orders regardless of side, plus the incoming order, read in O(1) from
`AccountEntry` (§4). A two-sided quote of 500k on each side consumes the full
1M cap. For a Market order, `price` in this formula is the reference price
above, not the wire value.

Net notional (bid minus ask) was considered and rejected. It does not bound
worst-case exposure — nothing prevents both sides of a two-sided quote filling
in quick succession during a fast move, which is real turnover the account
incurred while appearing flat. It also permits unbounded *gross* book presence
while net-flat, which is a market-integrity problem independent of risk. Gross
is also the cheaper implementation: one unsigned running total, no sign
tracking, no decision about whether the cap applies to net or its absolute
value.

Stored as `u128`. Realistic values are nowhere near overflowing `u64` — a $65,000
BTC price at 1 BTC is roughly 6.5e10 — so this is defensive against adversarial
or malformed input reaching the multiplication before validation catches it,
rather than a response to expected magnitudes. Silent wraparound inside a risk
check is a bad failure mode to leave available.

Breach rejects with `MaxOpenOrders` or `MaxNotional` — distinct reason codes,
not a generic reject.

### Price band

Reject with `PriceBandViolation` if an incoming order's price falls outside
**±10%** of the reference price. Reference resolution, in order:

1. The last trade price, if any trade has occurred.
2. Otherwise the book mid, **only if both sides have depth**.
3. Otherwise the check is skipped.

Case 3 covers both an empty book and a one-sided book — a mid is undefined with
only one side, and inventing one from a single side would produce a band derived
from the very orders it is supposed to constrain.

The band does not apply to `Market` orders. A market order can only fill against
orders that already rested, and every resting order passed the band check on
entry, so banded resting prices bound market-order fills transitively.

### Reason codes

A single `RejectReason` enum, exhaustive, each variant with a distinct wire
value: `MalformedMessage`, `UnknownMessageType`, `DuplicateOrderId`,
`UnknownOrderId`, `ZeroQuantity`, `WouldCross`, `NotFullyFillable`,
`KillSwitchActive`, `MaxOpenOrders`, `MaxNotional`, `PriceBandViolation`.

The README carries the full taxonomy as a table.

---

## 6. Verification

### Invariants — `assert_invariants()`

- No empty levels remain in either price map
- Cached `total_qty` and `count` match a walk of each level's chain
- Forward and backward links consistent; `tail` reachable from `head`
- Every node's side and price match the map entry it is filed under
- Every resting node has non-zero quantity; no zero-price orders
- Order index and book agree in both directions
- **Account index agrees with the book in both directions**: every node's
  `account` has an `AccountEntry` whose `slots[node.acct_idx]` is that node's
  slot; every slot in every `slots` array points to a live node owned by that
  account; `notional` matches a walk of the account's orders
- The book is neither crossed nor locked (best bid strictly < best ask)

### Required invariant tests

- No negative inventory; no orders at zero qty
- Resting quantity conserved across non-matching operations
- Every `Trade` has exactly one maker and one taker, with maker price = trade
  price
- **Price-time priority never violated under concurrent submission.** Tested
  with multiple client threads submitting concurrently through real sockets —
  genuine OS-level concurrency — asserting fills still reflect strict arrival
  order at the matching thread. The invariant holds *because* of single-writer
  architecture; the test proves the concurrency is real rather than avoided.
- Replay from a recorded stream produces identical output
- Each stream's `StreamSeq` is strictly monotonic **within that stream**
  (§2) — there is no single counter monotonic across streams

### Property tests

- **Quantity conservation**: `submitted == 2·filled + resting + cancelled +
  discarded`
- **Determinism**: a recorded command sequence replayed against a fresh engine
  produces a byte-identical event stream. Operation set covers every submit
  kind, cancel, modify, and mass-cancel.
- **No crossed or locked book** after any command
- **Invariants hold** after every command

### Wire protocol tests

Round-trip encode/decode for every message type; truncated frames; unknown tags;
out-of-range enum values; a frame split across two `read()` calls; two frames
arriving in one `read()`.

### Allocation tests

Two tests, because one number would hide the boundary described in §4:

- `hot_path_allocates_nothing` — warm the engine, reset the counter, then run a
  mixed submit/cancel/modify/mass-cancel sequence **confined to price levels
  that already exist**. Asserts exactly zero allocations.
- `level_churn_allocation_is_bounded` — deliberately creates and destroys price
  levels across a realistic band, and **records allocations per 1,000
  operations** rather than asserting zero. Its purpose is to quantify the rate,
  not to pass a threshold. The figure goes in `BENCH.md`.

A single test that happened to never create a level would report zero and be
misleading. The split makes the behaviour visible by construction, and the
per-1,000-operations unit is what makes the result comparable against a
near-zero target.

### FOK precheck and STP interaction

The FOK precheck must be **account-aware**: crossable depth belonging to the
aggressor's own account will be cancelled by STP, not filled, so it must not
count toward fillability. The precheck walks each crossing level's chain
excluding self-quantity, in the same order and bounded by the same orders a
successful match would touch, early-exiting as soon as enough foreign quantity
is found.

This is the one permitted exception to the no-linear-scan rule: FOK already has
a precheck-then-match structure precisely to buy certainty before mutating, and
the scan is bounded by what the match would touch anyway. It is not the hot path.
Documented as such in the README.

---

## 7. Deterministic replay

Given the same ordered input stream, output must be byte-identical: execution
reports and market data, including sequence numbers.

- No wall-clock in the matching core
- No `HashMap` iteration order reaching output
- No per-run randomness
- Engine-assigned timestamps excluded from comparison via a documented
  `--exclude-timestamps` flag

**Recording** captures the decoded, post-*gateway-validation* `Command`
sequence — i.e., after frame decode and the checks in §3 (length, enum range,
nonzero fields), before risk. **Replay** feeds that sequence through the full
risk-then-matching pipeline exactly as live traffic does; it bypasses only the
socket and framing layer, not risk.

This is a deliberate choice, not the only reading available. Feeding `Engine`
directly and skipping risk would also be internally consistent, but it proves a
strictly weaker property: it could not confirm that the kill switch, price
band, or notional cap reject identically on every replay, which are exactly the
cases deterministic replay is most valuable for verifying. Replaying through
risk means a command that was rejected live must be rejected identically on
replay, not silently accepted.

A recorded stream and the replay command ship in the repo.

---

## 8. Market data

- `BookUpdate` (top-of-book) on every book change; `Trade` on every execution
- Each carries its own `StreamSeq` (§2) — independent from execution
  reports' — with gap-detection hooks

**Backpressure: bounded channel, drop-oldest.** The matching thread must never
block on a slow subscriber — that would let market data stall the hot path and
defeat the point of single-writer design. A subscriber that falls behind loses
the oldest updates and detects the loss via a sequence-number gap.

Rationale for the README: this is the standard real-feed pattern, and it is
exactly what snapshot+delta recovery exists to handle — named as future work
rather than silently skipped.

---

## 9. Benchmarking

Measurement discipline matters more than absolute numbers. A p99 figure with no
stated methodology is not evidence.

### Tooling

| Crate | Role | Why |
|---|---|---|
| `criterion` | dev-dependency; micro-benchmarks | Statistical sampling with confidence intervals and outlier detection, rather than a single timed loop. Custom harness (`harness = false`). |
| `proptest` | dev-dependency; property tests | Generates randomised operation sequences and **shrinks** a failure to a minimal counterexample. Regression seeds are committed, so a found bug replays automatically forever. |
| `hdrhistogram` | dependency of `bench` only | Records the full latency distribution at fixed precision so p99.9 and p99.99 are real recorded quantiles, not estimates from mean and stddev. |
| `allocation-counter` | **optional** dependency behind `count-allocations` | Provides a safe `measure(closure)` API over a counting global allocator. Its `unsafe impl GlobalAlloc` lives in the dependency, not in this source — the same trust relationship as depending on `std`'s collections. |

The README explains this table, and specifically why the allocator crate is an
**optional** `[dependencies]` entry rather than a dev-dependency: its
`#[global_allocator]` registration is unconditional once compiled, and Cargo's
`optional = true` does not apply to dev-dependencies. Feature-gating compilation
is therefore the only lever that keeps the allocator override out of normal
builds.

```toml
[dependencies]
allocation-counter = { version = "0.8", optional = true }

[dev-dependencies]
criterion = "0.8"
proptest = "1"

[features]
default = []
count-allocations = ["dep:allocation-counter"]

[[bench]]
name = "matching_engine"
harness = false

[[test]]
name = "zero_alloc"
path = "tests/zero_alloc.rs"
required-features = ["count-allocations"]
```

A test in the normal (feature-less) suite shells out to `cargo tree` and asserts
`allocation-counter` is absent, so the gating is verified mechanically on every
plain `cargo test` rather than trusted.

### Micro-benchmarks (criterion)

On a pre-warmed book of 10 levels per side, 20 orders per level:
`submit_no_match`, `submit_new_level`, `submit_cross_one`, `submit_sweep_n`,
`cancel_random`, `modify_decrease`, `modify_increase`, `mass_cancel`.

### Hot-path latency (ingress → execution report emitted)

- **HDR histogram**, reporting **p50, p99, p99.9, p99.99** — not mean/stddev
- **Coordinated-omission handling**: latency measured against each message's
  *intended* send time under a fixed offered rate, not the time it was actually
  sent. A harness that only measures reply gaps hides queueing delay.
- **Warmup** phase excluded from the histogram, with the count stated
- **Burst workload**: 1M messages, configurable mix, default **70% new orders /
  20% cancels / 10% aggressive crossing**
- Sustained throughput (messages/sec) alongside the latency distribution

### Allocation count

Two measurements, matching the test split in §6:

1. **Zero**, asserted, for operations against existing price levels.
2. **Allocations per 1,000 hot-path operations** under the burst workload, which
   includes level creation and removal. This is the number that answers the
   near-zero target, and it is reported rather than asserted against a
   threshold.

`BENCH.md` reports both, and the tail-latency interpretation paragraph
references the second: an occasional B-tree node split on a new-tick submit is a
concrete, nameable tail source, and quantifying it is worth more than an
unqualified zero that a careful reader would distrust.

### Reporting

`BENCH.md` carries methodology, machine spec, warmup and sample counts, whether
pinning was enabled, and a paragraph interpreting **where the tail comes from**.
Any optimisation needs a recorded before/after delta or it gets reverted — and a
reverted optimisation gets documented as such.

---

## 10. CPU pinning, hugepages, NUMA

**CPU pinning is implemented and measured.** The matching thread is pinned to a
dedicated core (`core_affinity`, or `sched_setaffinity` under the `unsafe` FFI
comment rule). `BENCH.md` reports p99/p99.9 with and without pinning under the
same workload — a real before/after number, not a claim.

**Hugepages and NUMA are described, not implemented.** Rationale, stated plainly
in the README rather than left as a gap:

- Hugepages would reduce TLB pressure on the arena's frequently-touched pages
  under sustained load. Demonstrating this needs host-level `hugetlbfs`
  reservation, which conflicts directly with running from a clean clone with no
  setup.
- NUMA-aware placement only matters once sharding across sockets, so each
  shard's arena lives in its own node's memory. Demonstrating it needs
  multi-socket bare metal.
- Producing a number for either on unknown hardware would be noise presented as
  evidence — worse than no number.

---

## 11. Stages

The gate for each stage is its exit criteria. Do not start the next stage
without explicit approval. Commit at stage boundaries only.

### Core slice — scoped to roughly three hours of implementation

| Stage | Content | Exit criteria |
|---|---|---|
| **0** | Workspace, crate skeletons, domain types, reason codes, `check.sh`, Dockerfile + compose skeleton | `./check.sh` green; `cargo tree -p core` empty; `docker compose up` runs a stub |
| **1** | `core`: arena, levels, account index, matching, cancel, modify, mass-cancel, all TIFs, STP, `assert_invariants()`, scenario + conservation property tests | All §6 layers green for core behaviour |
| **2** | `wire`: message schemas, encode/decode, framing, round-trip and malformed-input tests | Every message type round-trips; all malformed cases rejected |
| **3** | `gateway` + `bin`: `Transport` trait, UDS impl, two-thread topology, execution reports, end-to-end order flow | An order sent over the socket produces a correct execution report |
| **4** | `risk`: kill switch (drain, checked in the matching loop), per-account limits, price band, full reason taxonomy | Each control has a scenario test with its distinct reason code |
| **5** | `marketdata`: top-of-book + trade prints, sequence numbers, drop-oldest backpressure, gap detection | A subscriber receives both streams; gap detection tested |
| **6** | Determinism: record + replay, byte-identical comparison, determinism property test, concurrent-submission priority test | Replay is byte-identical; concurrency test green |
| **7** | Benchmarks: criterion micro-benchmarks, HDR hot-path histogram with coordinated-omission handling, burst workload, zero-alloc test | Numbers recorded in `BENCH.md`; zero-alloc test green |
| **8** | README (architecture, wire tables, policies, reason taxonomy, microstructure write-up), `BENCH.md`, compose verified from clean clone | All doc deliverables complete; `./check.sh` green |

### Extension slice — deliberately beyond the core window

Each carries its rationale; each is a separate commit so the boundary is visible
in history.

| Stage | Content | Why beyond core |
|---|---|---|
| **9** | CPU pinning + before/after p99 measurement | Cheap, measurable, directly relevant to prior kernel-bypass and bare-metal work. Produces a measured number rather than a claim. |
| **10** | Documentation of the DPDK implementation path against the existing `Transport` trait, including where `unsafe` is confined and the RAII mbuf wrapper | Demonstrates the abstraction survives a move to kernel-bypass networking, with the boundary already built rather than promised. |
| **11** | Hugepages / NUMA write-up | Completes the latency-engineering story honestly, including why they are described rather than measured. |

**If time runs short**, cut in this order: stage 11, stage 10, stage 9, then
stage 7's criterion micro-benchmarks (keeping the HDR hot-path measurement,
which is a hard requirement). **Never cut stage 8.** A coherent, well-documented
slice is worth more than a sprawling half-working one.
