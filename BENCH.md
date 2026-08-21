# Benchmarks

Measurement discipline matters more than absolute numbers (SPEC §9). This
document states methodology alongside every figure, not the figure alone.

## Machine and build

| | |
|---|---|
| CPU | Apple M2 Pro, 10 cores (10 physical) |
| RAM | 32 GB |
| OS | Darwin 25.5.0 (macOS), arm64 |
| Rust | rustc 1.91.1, cargo 1.91.1 |
| Build profile | `cargo bench` (criterion, `opt-level` release-equivalent); `cargo run -p bench --release` |
| CPU pinning | **Enabled by default** (stage 9, SPEC §10): the matching thread pins itself to a dedicated core via `core_affinity` (`gateway::matching::pin_to_dedicated_core`), gated off by `MATCHING_PIN=0` for comparison. Real and enforced inside the Linux containers (`engine`/`dev`, `cpuset: "0-3"`); on the bare macOS host used for the earlier criterion/HDR figures above, `core_affinity`'s call succeeds but macOS is not required to honor the affinity hint the way Linux's `sched_setaffinity` does — those host numbers were never pinned in any enforced sense, pinning or not. See "CPU pinning: pinned vs. unpinned" below for the measured comparison and its caveats. |

Two independent measurement tools, deliberately different in shape:

- **Criterion micro-benchmarks** (`crates/core/benches/matching_engine.rs`)
  are in-process, calling `Book` directly — no socket, no gateway, no risk
  layer. They isolate the matching engine's own per-operation cost.
- **The HDR harness** (`cargo run -p bench`) is a real client of the same
  order-entry socket any other connection uses (SPEC §4): it spawns the
  actual daemon wiring (`bin::run`), connects over a real UDS socket,
  encodes real `wire::NewOrder`/`CancelOrder` bytes, and measures from each
  message's intended send time to receiving the real, decoded execution
  report back through the real gateway → matching thread →
  return-dispatcher pipeline. These two tools measure different things on
  purpose; neither number substitutes for the other.

### Docker reproduction

Both benchmark suites (and the full test suite) also run inside
`docker compose run --rm dev ...` (`dev` targets the `Dockerfile`'s
`builder` stage, pinned to rustc 1.91.1 to match the host exactly — see
the README's "Running, benchmarking, replaying" section). All three
commands confirmed clean: `./check.sh` (147 tests, exit 0),
`cargo bench -p core` (all 8 benchmarks, times consistent with the host
figures below), and the HDR harness (999,998/1,000,000 matched — the same
small-percentage-unmatched category as the host run below, not
investigated).

One real behavioral difference, not a correctness issue: **sustained
throughput came out nearly identical to the host (~28,442 msg/s vs.
~28,505 msg/s), but p50 latency was roughly 60x lower (117.951 µs vs.
7196.671 µs)**. Both environments bottleneck at essentially the same
sustained rate, but the queueing behavior leading up to that bottleneck
differs: a plausible explanation is a smaller effective socket send
buffer inside Docker Desktop's Linux VM, causing the sender's `write_all`
to throttle down toward the sustainable rate sooner rather than building
up the multi-millisecond backlog seen on bare-metal macOS. This is a
different execution environment producing a different latency profile at
the same throughput ceiling, not a version, dependency, or correctness
difference — flagged rather than smoothed over.

The criterion figures above (~150-300ns for `submit_no_match` etc.)
measure `Book::submit_gtc` called directly, in-process — no socket, no
thread hop, no syscalls. The HDR harness's ~35µs-per-message throughput
ceiling (1 / 28,500 msg/s) is a different measurement: a full round trip
crosses four thread wake-ups (client write → gateway thread's read wakes
→ channel send to the matching thread → channel send to the
return-dispatcher thread → client's reader thread wakes), each
individually costing more than the entire `Book` call it wraps. The
matching logic itself is roughly 1% of the per-message budget; the other
99% is socket syscalls and OS thread scheduling. Busy-spinning the
matching thread (SPEC §4) removes exactly one of those four wake-ups, by
design — the other three (gateway's read, the dispatcher's channel
receive, the client's read) are still blocking waits. This is the
architectural price of single-writer correctness enforced over real
sockets, not a discrepancy between the two measurements. It's the same
cost class DPDK/kernel-bypass (SPEC §10) is named as future work to
attack, by removing socket syscalls from the path entirely rather than
reducing wake-ups one at a time.

---

## Micro-benchmarks (criterion)

Pre-warmed book: 10 price levels per side, 20 orders per level (400 resting
orders), rebuilt fresh via `iter_batched` for every sample — required for
correctness, not just style, since `cancel_random`/`modify_*`/crossing
benchmarks all mutate state later iterations depend on being present.
Criterion defaults: 3s warmup per benchmark (excluded from the reported
distribution), 100 samples collected per benchmark.

| benchmark | time | what it does |
|---|---|---|
| `submit_no_match` | 138.82 – 146.73 ns | GTC order appended to an existing level, no crossing |
| `submit_new_level` | 145.24 – 151.19 ns | GTC order at a price between two existing levels |
| `submit_cross_one` | 165.27 – 184.86 ns | crosses and fully consumes exactly one resting order |
| `submit_sweep_n` | 3.1276 – 3.1725 µs | large qty, sweeps several resting orders across levels |
| `cancel_random` | 198.51 – 212.40 ns | cancels an order from the middle of a level's queue |
| `modify_decrease` | 116.75 – 129.01 ns | quantity decrease, retains queue priority, in place |
| `modify_increase` | 237.42 – 276.48 ns | quantity increase, loses priority, re-enters at the back |
| `mass_cancel` | 1.2943 – 1.3423 µs | one account, resting orders across all 20 levels |

`submit_sweep_n` and `mass_cancel` are the two orders-of-magnitude-larger
figures, and both are explained by what they do, not by anything
surprising: `submit_sweep_n` touches ~3 levels' worth of resting orders
sequentially; `mass_cancel` walks and unlinks every one of an account's
resting orders (20, spread across all 20 warmed levels in this benchmark).

---

## Hot-path latency (HDR harness, burst workload)

### Coordinated-omission methodology

Every message's latency is measured against its **intended** send time on a
fixed schedule (`start + i × interval`) computed once up front — never
against when it was actually sent, and never against the previous reply's
arrival time. A single sender thread paces against this fixed schedule on
one pipelined connection, never waiting for a reply before proceeding to
the next scheduled slot; if the system falls behind, the sender keeps
pacing against the original schedule regardless, so the resulting queueing
delay shows up in the measured latencies rather than being silently
absorbed by a sender that slows down to match. A separate reader thread
drains the same socket and correlates each reply back to its intended send
time via `(AccountId, OrderId)` (not send-order/reply-order position,
which breaks the moment a single command produces more than one event —
a crossing order's `Filled` followed by its own `Accepted`).

### Workload composition

Default mix 70% new orders / 20% cancels / 10% aggressive crossing (SPEC
§9), configurable via `--new-pct`/`--cancel-pct`/`--cross-pct`. Two
disclosures on how this workload was built:

1. **The aggressive-crossing 10% is split 5%/5% between `Market` and a
   large-qty GTC `Limit`**, not entirely `Market`. `Market` always behaves
   as IOC and discards any remainder (SPEC §2) — an all-`Market` crossing
   bucket could never produce `Accepted{resting_qty>0}` after a partial
   sweep, so the "cross several levels, then rest what's left" path would
   never appear in this measurement at all. The other half is a GTC
   `Limit` priced at the far edge of the opposite side's resting-level
   range, large enough to plausibly cross several levels and still rest a
   remainder, specifically to exercise that path under live measurement.
2. **`risk-bench.toml`, not production `risk.toml`.** `max_open_orders`
   raised from 50 to 1,000,000 and `max_notional_ticks` from 100,000,000
   to 1,000,000,000,000 — high enough that the burst's account population
   never approaches either cap. This is a deliberate methodology choice:
   the harness measures matching hot-path latency, and production's
   tighter limits would flood a sustained 1M-message burst with
   risk-rejects, contaminating the distribution with reject latency
   instead of the order-processing latency the mix is meant to exercise.
   **These figures do not characterize behavior under production's
   tighter caps, or under sustained cap-boundary conditions** — that's a
   different, not-yet-measured question. `price_band_pct` was left at the
   production value (10), deliberately, not by oversight: the workload's
   resting prices (9950–10050, five levels per side) stay within a ~1%
   band of each other and of any `last_trade` the run can produce, so the
   production band never rejects legitimate workload traffic and didn't
   need relaxing.

### Warmup and sample counts

First 50,000 messages sent and processed identically to the rest, but
excluded from the histogram (their replies are still correlated and
counted, just not recorded into it).

### Results

Default config: 1,000,000 messages, offered rate 200,000 msg/s.

```
sent:              1,000,000
matched:             999,975
unmatched:                25
warmup excluded:      50,000
histogram samples:   949,975

p50:      7196.671 us
p99:      7688.191 us
p99.9:    7901.183 us
p99.99:   7970.815 us
max:      8048.639 us

wall clock: 35.081 s
sustained:  28,505 msg/s
```

**25 of 1,000,000 messages (0.0025%) never received a matched reply**
within the harness's 30-second idle timeout. This is disclosed as a very
small percentage and explicitly **not** root-caused — investigating it is
out of scope for this stage. The harness's accounting makes this
impossible to hide: `sent`/`matched`/`unmatched` are always printed as
hard counts, not only when something is wrong, specifically so a lossy run
can never look identical to a clean one.

### CPU pinning: pinned vs. unpinned (stage 9)

The matching thread pins itself to a dedicated core via `core_affinity`
(`gateway::matching::pin_to_dedicated_core`) — chosen over a raw
`sched_setaffinity` FFI call specifically so this codebase needs no
`unsafe` at all for it, even though CLAUDE.md would permit `unsafe` here
as the one named FFI/hardware-boundary exception. The pin target is
simply the *first* core id the OS reports as available to the process —
taken, not chosen for any property of that core; the pin-success log line
(`matching-engine: matching thread pinned to core N`) reports whichever id
that turned out to be, not a selection made for a reason.

Pinning is on by default and gated off by an environment variable read
once at matching-thread startup, `MATCHING_PIN=0`, so both distributions
below come from **one binary**, reproducible verbatim by anyone with these
two commands — not two separately-built binaries from a temporarily
edited call site, which nobody could rebuild identically afterward:

```sh
docker compose run --rm dev cargo run -p bench --release -- --messages 1000000
docker compose run --rm -e MATCHING_PIN=0 dev cargo run -p bench --release -- --messages 1000000
```

Run inside the `dev` container, not the bare macOS host, because real,
*enforced* pinning is Linux-only: `sched_setaffinity` (which
`core_affinity` calls under the hood on Linux) is honored by the kernel;
on bare macOS, the equivalent call reports success but the OS is not
required to actually honor the affinity hint. `docker-compose.yml` gives
both `engine` and `dev` `cpuset: "0-3"` — four cores, not one, because the
daemon runs four threads (gateway, matching, market-data,
return-dispatcher) plus the runtime; a single-core cpuset would force all
of them to timeshare one core, which is contention, not a dedicated core,
and would make the pinned run look *worse* than unpinned for reasons
having nothing to do with pinning.

**Three runs of each, same command, same container, same cpuset, same
1,000,000-message/200,000 msg/s/50,000-warmup workload:**

| run | condition | matched/sent | wall clock | sustained | p50 (µs) | p99 (µs) | p99.9 (µs) | p99.99 (µs) | max (µs) |
|---|---|---|---|---|---|---|---|---|---|
| 1 | pinned | 1,000,000 / 1,000,000 | 5.000 s | 199,993 msg/s | 103.551 | 220.543 | 527.871 | 1116.159 | 1343.487 |
| 2 | pinned | 1,000,000 / 1,000,000 | 5.000 s | 199,993 msg/s | 103.807 | 225.023 | 550.911 | 1028.607 | 1191.935 |
| 3 | pinned | 999,999 / 1,000,000 | 35.154 s | 28,446 msg/s | 104.383 | 224.895 | 342.271 | 893.951 | 1203.199 |
| 1 | unpinned (`MATCHING_PIN=0`) | 999,998 / 1,000,000 | 35.155 s | 28,445 msg/s | 104.639 | 218.239 | 311.295 | 823.295 | 1121.279 |
| 2 | unpinned (`MATCHING_PIN=0`) | 1,000,000 / 1,000,000 | 5.000 s | 199,998 msg/s | 102.719 | 218.111 | 314.623 | 705.535 | 861.183 |
| 3 | unpinned (`MATCHING_PIN=0`) | 1,000,000 / 1,000,000 | 5.000 s | 199,993 msg/s | 102.847 | 222.719 | 587.775 | 2846.719 | 3291.135 |

**Honest reading: no consistent tail-latency benefit from pinning is
observable in this environment, and one larger, unrelated effect swamps
it.** Two things stand out, and neither is the clean "pinning helps the
tail" story stage 9 might have hoped to find:

1. **A throughput bimodality — 5.000s/~200,000 msg/s vs. ~35s/~28,400
   msg/s — occurs under *both* conditions, uncorrelated with pinning.**
   Runs 1–2 pinned and run 2–3 unpinned hit the fast mode; run 3 pinned
   and run 1 unpinned hit the slow mode. If pinning caused this, it would
   show up only on one side; it doesn't. This is very likely the same
   effect BENCH.md's Docker-reproduction section already named (a smaller
   effective socket send buffer inside Docker Desktop's Linux VM changing
   queueing behavior run to run) rather than anything stage 9 introduced.
   Disclosed here with hard numbers, not root-caused — a deeper
   investigation into what specifically flips this mode is out of scope
   for this stage, same discipline as the 25-message unmatched rate above.
2. **Within the fast-throughput mode, run-to-run tail variance is larger
   than any pinned-vs-unpinned difference.** Comparing only the four
   fast-mode runs (pinned 1, 2; unpinned 2, 3): p50 and p99 are
   statistically indistinguishable across all four (103–104 µs / 220–225
   µs pinned vs. 103–103 µs / 218–223 µs unpinned — a ~1–2% difference,
   well inside normal jitter). At p99.9/p99.99/max, pinned runs sit at
   528–551 / 1029–1116 / 1192–1343 µs, while unpinned runs range from a
   *better* 315 / 706 / 861 µs (run 2) to a *worse* 588 / 2847 / 3291 µs
   (run 3) — a single unpinned run has the single worst tail in the whole
   table. Three samples per condition is not enough to distinguish a real
   effect this size from noise of this size, and this data doesn't try to.

This is consistent with, not contrary to, the caveat named going in:
**Docker Desktop's Linux VM has its own guest vCPUs scheduled onto
physical Apple Silicon cores by the macOS hypervisor underneath it.** The
pin is real and enforced at the *guest kernel* level — `sched_setaffinity`
genuinely restricts the matching thread to one guest vCPU, confirmed by
the `matching thread pinned to core N` log line appearing in every pinned
run — but that guest vCPU is not itself guaranteed a fixed *physical*
core the way bare-metal Linux with `isolcpus` would give. A guest-level
pin that the hypervisor can still freely move across physical cores
underneath is a substantially weaker claim than the "no scheduler
migration, no preemption jitter" story pinning tells on bare metal, and
the data above is consistent with that weaker claim actually holding: the
guest-level affinity is real, but it isn't preventing the migration/
preemption jitter it exists to prevent, because the layer doing the
migrating (the hypervisor) is one level below what this pin controls.
Measuring this properly would need bare-metal Linux with `isolcpus`
reserving a genuinely fixed physical core — out of scope for what this
development machine can produce.

---

## Allocation

Behind the `count-allocations` feature (`allocation-counter`, optional, in
`[dependencies]` — `optional = true` doesn't work on `[dev-dependencies]`,
and the crate registers `#[global_allocator]` unconditionally once
compiled, so feature-gating compilation is the only lever that keeps it
out of normal builds). `crates/risk/tests/dependency_hygiene.rs` proves
this mechanically in the plain test suite: it shells out to `cargo tree`
and asserts `allocation-counter` is absent. Confirmed absent from a full
`./check.sh` run with no features passed, and present only when
`--features count-allocations` is passed explicitly.

Both tests (`crates/risk/tests/zero_alloc.rs`) route commands through
`risk::process_command`, not `Engine::apply` directly — CLAUDE.md's
hot-path rule explicitly includes the risk check in what must not
allocate, and `process_command` is the exact function the live matching
thread and replay (stage 6) both already call; measuring `Engine::apply`
alone would silently skip proving the risk layer is allocation-free.

### `hot_path_allocates_nothing` — asserts exactly zero

Covers the full order-type/TIF variety against existing accounts and
existing price levels: GTC, IOC, FOK, PostOnly, Market, a genuine
STP-triggering cross (an account crossing its own resting order, isolated
at a price no other order shares, so price-time priority can't route the
match to someone else first), a plain cancel, both modify directions, and
a mass-cancel. Result: **zero allocations.**

**A real allocation was found and fixed while building this test, not a
warm-up artifact.** `Book::mass_cancel` did `let snapshot: Vec<u32> =
entry.slots.clone()` — a fresh heap allocation on every call, needed to
end the borrow on `self.accounts` before mutating via `unlink`. Fixed with
a new `Book` field, `mass_cancel_scratch: Vec<u32>`, reserved once and
reused forever after — the same "grow once, keep the capacity" shape
`AccountEntry::slots` already used: `clear()` then `extend_from_slice`
ends the same borrow without allocating once the buffer has grown to
cover the largest mass-cancel `Book` has handled so far. Verified by
reverting the fix and confirming the test failed again identically
(`count_total: 1, bytes_total: 4` for a single-order mass-cancel), then
restoring it.

**Warm-up design.** Every account and price level the measured sequence
touches is pre-warmed by running the *entire* measured command shape once,
unmeasured, against the same accounts and the same price levels (fresh
order ids the second time, so nothing rejects as a duplicate) — not by
pre-warming accounts individually. This is sound for a structural reason,
not merely because it happened to work: every allocating structure this
hot path touches — `AccountEntry::slots` (reserved once per account, on
first rest), each price level's `BTreeMap` entry (created once per
distinct price), `Arena`'s slot storage (grows only past every previous
peak *concurrent* occupancy, since freed slots return to a reused free
list), `mass_cancel_scratch` (grows only past every previous peak
mass-cancel size), and `Book::accounts`'s `HashMap` bucket array (grows
only as the *count* of distinct accounts crosses a threshold, and no
account is ever removed) — is monotonic and one-directional: capacity,
once granted for a given key or a given peak size, is never released.
Running the identical operation shape twice against the identical account
and price-level set guarantees the second run introduces no new key and
no new peak, regardless of which specific structure's growth policy would
otherwise have been responsible — which is exactly what let this
generalize past a real surprise encountered while building it: an
account-by-account warm-up still measured a stray allocation on `Modify
increase`, traced (by isolating every command with its own measurement,
then toggling warm-up pieces on and off) to a `HashMap` resize on
`Book::accounts` triggered by whichever account happened to push the map
over a growth threshold, not a new bug.

### `level_churn_allocation_is_bounded` — records, doesn't assert zero

10,000 submit operations across a sliding 100-price window (creating and
destroying levels at the edges as it moves), plus the cancels that empty
each level behind it — 19,900 total operations.

```
28 allocations over 19,900 operations = 1.4070 per 1,000 operations
```

Reported per 1,000 operations, not per level created: `BTreeMap` only
allocates on a node split (B=6, up to ~11 entries per node), not on every
key insert, so a per-level figure would overstate the true rate.

---

## Known limitation: `Book::accounts` under continuous account churn

`hot_path_allocates_nothing`'s two-pass design proves the hot path is
allocation-free *given a stable, already-seen account population* — it
does not prove anything about a `HashMap` bucket-array resize on
`Book::accounts` under **continuous** account arrival, because neither
allocation test exercises that shape. This is worth naming explicitly
rather than letting the clean pass above imply it isn't a concern.

Continuous account arrival throughout a session — new participants
connecting over the session's lifetime, not just at startup — is the
realistic shape for a production matching engine; nothing in SPEC.md
bounds the account universe to what's known at boot. The HDR harness's own
workload (`crates/bench/src/workload.rs`) uses a fixed 256-account
round-robin pool established early and reused for the rest of the run,
which approximates "stable after startup" — a property chosen for that
synthetic workload, not evidence about production traffic, and the
distinction matters here specifically.

`Book::accounts` is constructed via a bare `HashMap::new()`, with no
pre-sizing — unlike gateway's comparable `resting_conn` table
(`crates/gateway/src/matching.rs`), which already carries a documented
sizing heuristic (`RESTING_CONN_INITIAL_CAPACITY`) for exactly this
reason. And unlike `Arena`, which reuses freed slots via a free list,
nothing ever removes an `AccountEntry` once created, so `Book::accounts`
only ever grows. An occasional bucket-array rehash — copying every
existing entry into a new array — is the same *category* of allocation as
a `BTreeMap` node split on level churn (amortized, not a per-call cost
like the old `mass_cancel` clone was), but unlike the price-level
`BTreeMap`, which is bounded by the number of live price ticks and
realistically small and stable, the account universe over a long-running
session could plausibly grow into the hundreds of thousands — so
individual rehash events get progressively more expensive even as they
get rarer. Not measured, not fixed here; the concrete next step is an
`ACCOUNTS_INITIAL_CAPACITY` constant passed to `HashMap::with_capacity`
at `Book::new()`, mirroring the precedent `RESTING_CONN_INITIAL_CAPACITY`
already set for `resting_conn` — a documented sizing heuristic against an
expected account-population order of magnitude, not a hard bound, the
same tradeoff gateway already accepted for the comparable table. Paired
with its own allocation test exercising sustained account arrival (not
just level churn) in a future stage.

---

## Where the tail comes from

Two distinct, independently-measured sources of tail latency, not one:

**Allocation-driven tail.** `level_churn_allocation_is_bounded`'s 1.4070
allocations per 1,000 operations is the concrete, nameable source SPEC §9
asks for: an occasional `BTreeMap` node split on a new-tick submit,
amortized and small, converging toward zero in steady state as the tree
structure stabilizes around the active price band. This is a real,
bounded, quantified cost — not an unqualified zero a careful reader would
distrust, and not (per the section above) a complete accounting, since
account-churn-driven `HashMap` resizing is the same category of cost,
currently unmeasured.

**Queueing-driven tail.** The HDR burst's own tail (p50 7196.671 us
through max 8048.639 us — tightly clustered, not growing unboundedly
across the 35-second run) is a different phenomenon from allocation
entirely: sustained throughput (28,505 msg/s) came in far below the
200,000 msg/s offered rate, and the narrow, stable latency band across the
whole run is consistent with the pipeline (bounded 1024-capacity command/
return/market-data channels, a single reader thread, a single matching
thread, a single return-dispatcher thread) reaching a stable backpressure
equilibrium well below the offered rate — the sender's own `write_all`
blocking on a full socket send buffer once that equilibrium is reached.
This is exactly the queueing delay coordinated-omission-safe measurement
exists to expose rather than hide, not evidence of a per-operation
allocation cost; the ~7-8ms figures are latency-under-load, not
processing time (the criterion micro-benchmarks above put actual
per-operation processing time at 100ns–3µs). No optimization has been
attempted against either source — this stage is measurement only.
