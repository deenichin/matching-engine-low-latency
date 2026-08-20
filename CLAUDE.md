# matching-engine

Single-symbol CLOB matching engine with a binary wire protocol, deterministic
replay, tail-latency measurement, and risk/operational controls.

Requirements: @SPEC.md. Staged build with gates: @PLAN.md.

## Commands

- `cargo test --workspace` — full suite
- `cargo test -p <crate> <name>` — single test. Prefer this while iterating.
- `cargo clippy --workspace --all-targets -- -D warnings` — must pass before any commit
- `cargo fmt --check` — must pass before any commit
- `cargo bench` — criterion micro-benchmarks
- `cargo test --features count-allocations --test zero_alloc` — allocation proof
- `cargo run -p bench --release -- --messages 1000000` — burst workload, HDR output
- `./check.sh` — fmt, clippy, tests. Run this before saying work is done.
- `docker compose up --build` — must work from a clean clone with no host setup

## Hard rules

- **No floating point in `core`, `wire`, or `risk`.** Prices are integer ticks,
  quantities integer lots. Floats appear only in benchmark reporting.
- **`core` has zero dependencies.** Not `wire`, not `libc`, nothing in
  `[dependencies]`. If a change would add one, stop and raise it. This is a
  verifiable claim in the README (`cargo tree -p core`), not a preference.
- **Single-writer discipline.** Exactly one thread ever calls `Engine::apply`.
  No `Mutex`, no `RwLock`, no `Arc<Engine>`. Anything needing book state gets it
  from the event stream, not a shared reference.
- **Cancel, modify, and per-account risk checks must be O(1).** No operation may
  scan a price level linearly, and no operation may walk the book to find an
  account's orders. The one documented exception is FOK's account-aware
  precheck (SPEC §6), bounded by exactly the orders a successful match would
  touch.
- **No heap allocation on the hot path against existing price levels.** Decode,
  risk check, match, and encode on a warm engine must not allocate when the
  operation touches only price levels that already exist. Buffers are
  caller-owned and reused. Channels are bounded and pre-allocated, and carry
  individual events, never a `Vec<Event>` — a per-dispatch collection defeats
  this rule regardless of how the channel itself is provisioned. Per-account
  slot arrays are reserved at account creation and never grow.
- **Price-level creation and removal may occasionally allocate.** `BTreeMap`
  allocates only when a node splits (B=6, so up to 11 entries per node), not on
  every key insert, and only deallocates on a merge. The rate is therefore well
  below one allocation per level created, and converges toward zero in steady
  state as the tree structure stabilises around the active price band. This is a
  known, measured exception: it is reported in `BENCH.md` as allocations per
  1,000 hot-path operations. Do not claim an unqualified zero, and do not widen
  this exception to cover anything else.
- **`unsafe` is permitted only at FFI or hardware boundaries**, with a comment
  naming the invariant upheld. Never in matching logic, wire decoding, or risk
  checks. The only current candidate is CPU affinity. Anything else — stop and
  raise it.
- **No wall-clock in the matching core.** Sequence numbers are engine-assigned
  and monotonic. Timestamps are stamped at the gateway only, and excluded from
  replay comparison via the documented flag.
- **No `HashMap` iteration order may reach output.** Iterating a `HashMap` to
  produce events or market data breaks byte-identical replay. Use an ordered
  structure or an explicitly ordered traversal.
- **`OrderId` is never a standalone map key.** It is unique per account, not
  globally (SPEC §2). Every index, lookup, and duplicate check uses
  `(AccountId, OrderId)`. If you find yourself writing `HashMap<OrderId, _>`
  anywhere, stop — that is the bug SPEC §4 was amended to prevent.
- **No `unwrap()`/`expect()` on anything derived from wire input.** Permitted
  only for internal invariants that cannot fail, each with a naming comment.
- **Node size discipline.** `Node` is intended to stay within one 64-byte cache
  line. Before adding a field to it, raise the size impact rather than adding
  silently — matching sweeps touch nodes sequentially and this dominates.

## Testing rules

- **Never weaken, delete, or `#[ignore]` an assertion to make a test pass.** Fix
  the code. If you believe the assertion is wrong, stop and say so.
- `assert_invariants()` must pass after every book-mutating operation in tests.
- Every new behaviour needs a named scenario test AND coverage in the relevant
  property test's operation set.
- Malformed wire input needs tests: truncated frames, unknown tags, out-of-range
  enums, zero qty. Each rejects with a specific reason code and never reaches
  `Engine::apply`.
- Benchmarks are not tests. A performance change needs a recorded before/after
  number in the commit message.

## Workflow

- Plan mode for anything touching more than one crate. Present the plan, wait.
- After implementing, run `./check.sh` and show me the output. Never report
  success without evidence.
- Commit at every stage gate in PLAN.md, never mid-stage. Conventional messages.
- If a stage's exit criteria aren't fully met, say so explicitly. Do not start
  the next stage without me saying so.
- If you hit the same error twice, stop and describe what you've tried.
- If a SPEC.md requirement turns out ambiguous or wrong mid-build, stop and raise
  it rather than picking an interpretation silently.

## Style

- Public items get doc comments explaining *why*, not *what*.
- Named constants, never magic numbers.
- Comments explain non-obvious decisions and domain rules. Don't narrate code.
