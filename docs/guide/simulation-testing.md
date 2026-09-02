# Simulation Testing

Autumn ships a **deterministic simulation testing (DST)** harness: a `#[sim_test]`
attribute that hands your test a seeded [`Sim`] handle, a paused deterministic
executor, and a virtual clock, so your whole app's concurrency — retries,
scheduled ticks, background jobs — runs identically on every machine and every
run. A failure prints a copy-pasteable line that reproduces it exactly, and a
seed-sweep runner (`sweep_proptest`, see below) lets you drive *your own*
workload against many seeds looking for rare-interleaving bugs a single-seed
test would never stumble into. **This sweeping is opt-in, not automatic**:
autumn's own CI runs a built-in `sim-sweep` job as a smoke check of the
harness mechanism itself (a small, fixed toy scenario over a few hundred
seeds) — it does not explore your application's interleavings. Wiring
`sweep_proptest` against your own scenario, in your own CI, is how you get
that coverage for your app; see "Property-based op-driving + the seed sweep"
below.

```
┌──────────────────┐
│   #[sim_test]     │  seed → deterministic Sim
└─────────┬─────────┘
          │
  ┌───────▼────────┐   paused, current-thread   ┌──────────────────┐
  │  Sim::advance / │ ─────────────────────────► │  your app's real  │
  │  run_to_idle    │      virtual clock          │  jobs/scheduler/  │
  └───────┬─────────┘                             │  request path     │
          │ panic (always! violated)              └──────────────────┘
  ┌───────▼────────────────────────────┐
  │ AUTUMN_SIM_SEED=0x… cargo test …   │  ← replay line, prints on failure
  └─────────────────────────────────────┘
```

---

## Why simulation testing

The nastiest production bugs are not logic errors in a single handler — they
are *interleaving* bugs: a retry storm when a downstream stalls, a deadlock
between a request and a scheduled tick, a timeout that fires in the wrong
order. These bugs need many actors racing at the same instant to trigger, so a
real-clock test would need to get impossibly lucky to reproduce one — and even
luckier to rerun it to prove a fix. A deterministic simulation makes "many
actors racing at the same instant" the cheap, constructible *default*: pause
the clock, drive everything from one seed, and the same instant is trivial to
manufacture on purpose.

## Quick start

`#[sim_test]` needs no extra feature flag — it's part of the default `autumn-web`
surface.

```rust
use autumn_web::sim::Sim;
use autumn_web::sim_test;

#[sim_test]
async fn deterministic(mut sim: Sim) {
    // The seed comes from `AUTUMN_SIM_SEED` (hex `0x..` or decimal), default 0.
    // `sim`'s own clock and RNG (`sim.rng()`) are pure functions of this seed.
    // An app mounted with `sim.build(...)` inherits the seeded clock
    // automatically, but NOT seeded entropy — see "Deterministic
    // identifiers" below for what that means and how to opt in.
    assert_eq!(sim.seed, 0);
}
```

Reproduce a failing run by copying the replay line printed on panic:

```text
AUTUMN_SIM_SEED=0x9f3a cargo test -p my-crate deterministic
```

### Mounting a real app

```rust
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::test::TestApp;

#[sim_test]
async fn app_boots_under_the_sim(mut sim: Sim) {
    sim.build(TestApp::new().routes(routes![index]).jobs(jobs![send_receipt]));

    let response = sim.client().get("/").send().await;
    response.assert_ok();
}
```

`sim.build` mounts an [`autumn_web::app::AppBuilder`]-configured app on the
sim's paused runtime, wired to the virtual clock and (if configured) the
fault-injection hooks below. **Seeded entropy is not automatic** — see
"Deterministic identifiers" below for how (and why) to wire it in
explicitly.

### Virtual time

```rust
sim.advance(std::time::Duration::from_secs(24 * 3600)).await; // 24h, zero wall-clock sleep
sim.run_to_idle().await; // drain everything the advance released
```

- [`Sim::advance`] steps the injected [`autumn_web::time::Clock`] and tokio's
  paused timer wheel together, so a `#[job]`'s exponential backoff or a
  `#[scheduled]` tick fires the instant virtual time crosses its deadline —
  with zero real waiting.
- [`Sim::run_to_idle`] drains everything already-ready — the job worker, due
  scheduler ticks, durable repository commit hooks — until the runtime is
  quiescent. It does **not** fast-forward to a future timer; pair it with
  `advance` for "jump to the next interesting instant, then settle what fired."
- [`Sim::advance_to`] / [`Sim::advance_to_local`] jump to a specific
  (optionally timezone-zoned, DST-aware) instant instead of a raw duration —
  useful for business-calendar / SLA tests that must cross a spring-forward or
  fall-back boundary deterministically.

### Deterministic identifiers

Framework-minted IDs (job IDs, request IDs, idempotency keys, session tokens)
draw from an injected [`autumn_web::entropy::Entropy`] source. Reach a seeded
one from a handler via the [`autumn_web::entropy::Rng`] extractor, or from the
sim directly via `sim.rng()`.

**Unlike the clock, entropy injection is opt-in** — `Sim::build` does not wire
it into the mounted app automatically. If any code path your test exercises
(a job's retry jitter, a minted UUID, anything reading `state.entropy()`)
needs to be reproducible from the seed, mount with it explicitly:

```rust
use autumn_web::entropy::SeededEntropy;

sim.build(
    TestApp::new()
        .routes(routes![index])
        .with_entropy(SeededEntropy::new(sim.seed)),
);
```

Skipping this is easy to miss and easy to get wrong silently: the app still
runs, the handler still reads *some* entropy source, and a test that only
asserts non-vacuity (e.g. "outcomes vary across N draws") still passes — it
just isn't actually replaying from `AUTUMN_SIM_SEED` anymore, so a failure
downstream of that draw won't reproduce from the printed seed (Codex review).

### Fault injection

`Chaos` injects faults **probabilistically** — you give it a rate and the seed
decides which operations get hit:

```rust
use autumn_web::sim::Chaos;

sim.chaos(
    Chaos::default()
        .db_transient_errors(0.05)     // 5% of connection checkouts fail
        .job_duplicate_delivery(0.10)  // occasional at-least-once double-delivery
        .clock_skew(std::time::Duration::from_millis(250)),
);
```

Every fault decision is drawn from the seed, so enabling chaos never breaks
reproducibility — the same seed replays the same fault schedule. See
[`autumn_web::sim::Chaos`] for the full catalog (SMTP transport faults, a
seeded LLM stub for agent retry paths, and mid-transaction kill/restart for
durable-recovery proofs). When you want to *name* a fault rather than sample
one — "fail the 3rd database checkout" — reach for `FaultPlan` in the next
subsection instead.

### Authored fault scenarios (`FaultPlan`)

`Chaos` is a *rate*: "5% of checkouts fail", with the seed choosing which ones.
That is the right shape for a sweep hunting rare interleavings, and the wrong
shape for a regression test — because the sentence a post-mortem actually
produces is "the **third** connection checkout failed while the **second**
`send_invoice` execution was retrying", and no probability reproduces that on
purpose. [`autumn_web::sim::FaultPlan`] lets you author that scenario directly,
by ordinal rather than by rate, and hands back a serializable record of what
happened that a test can assert on and CI can replay byte-for-byte.

```rust
use autumn_web::sim::{FaultPlan, Sim};
use autumn_web::sim_test;
use autumn_web::test::TestApp;

#[sim_test]
async fn the_third_checkout_and_the_second_invoice_fail(mut sim: Sim) {
    let plan = FaultPlan::from_seed(sim.seed)
        .fail_db_checkout(3)           // 3rd checkout on any pool (ordinals are 1-based)
        .fail_job("send_invoice", 2);  // 2nd execution of that job by name

    sim.build(
        TestApp::new()
            .routes(routes![checkout])
            .plugin(InvoiceJobPlugin)
            .with_fault_plan(plan),
    );

    for _ in 0..5 {
        sim.client().post("/checkout").send().await;
    }
    sim.run_to_idle().await; // drains the job worker through the fault seam

    let outcome = sim.client().fault_outcome().await;

    assert_eq!(outcome.fired.len(), 2);
    assert_eq!(outcome.fired[0].ordinal, 3);            // the checkout that failed
    assert_eq!(outcome.server_errors[0].status, 503);   // captured through reporting
    assert_eq!(outcome.final_state.db_checkouts, 5);    // every checkout, fired or not

    // Canonical, byte-identical on every replay of this seed — commit it as a
    // fixture and this scenario becomes a CI regression test.
    assert_eq!(outcome.to_json_string(), include_str!("fixtures/invoice_scenario.json").trim_end());
}
```

Ordinals are **1-based**, and `0` matches nothing (the same convention as
`Chaos::smtp_faults`). `fail_db_checkout(n)` and `fail_job_execution(n)` count
across every pool and every job name; `fail_db_checkout_on("replica", 2)` and
`fail_job("send_invoice", 2)` count on that target's own counter. Duplicate
entries collapse, so a plan is a set — `plan.planned()` returns the whole
schedule sorted and `plan.describe()` prints it one fault per line, both before
you run anything. A plan is pure data: clone it onto two apps and you get two
independent ledgers.

Faults can be confined to a window of virtual time, driven by the injected
clock rather than wall time:

```rust
let plan = FaultPlan::from_seed(sim.seed)
    .fail_job_execution(2)
    .only_between(Duration::from_secs(5), Duration::from_secs(10));
```

The window is half-open (`from <= elapsed < to`) and `elapsed` is measured on
the app's injected monotonic clock since the app started — so `sim.advance`
moves it and no real time ever does. Outside the window the effect still
consumes its ordinal, and the near-miss is recorded in `outcome.suppressed`
rather than vanishing: a scenario that stopped firing because your timings
shifted is visible in the outcome instead of silently passing.

The seed earns its keep in the `random_*` lane, which spreads faults across a
range instead of naming each one:

```rust
let plan = FaultPlan::from_seed(0x5EED).random_db_checkout_faults(2, 1..=8);
```

That picks 2 distinct checkout ordinals from `1..=8` using the plan's seed, and
resolves them into explicit entries *at builder-call time* — so `planned()`
still describes the schedule completely, a different seed picks different
checkouts, and a plan built only from explicit `fail_*` calls draws no entropy
at all.

#### What the outcome record holds

| Field | Contents |
|---|---|
| `seed` | the plan's seed, echoed so an outcome identifies its own replay |
| `fired` | `Vec<FiredFault>` in fire order: `effect`, `target` (pool or job name), the global `ordinal`, the `target_ordinal`, `at` (injected wall clock) and `elapsed_ms` (injected monotonic) |
| `suppressed` | matched an ordinal but fell outside `only_between` |
| `unfired` | planned faults the run never reached, sorted — an empty list (together with an empty `suppressed`) is the proof your scenario actually exercised what it authored; a near-miss outside the window counts as reached, so check `suppressed` too |
| `server_errors` | 5xx captured through `reporting.rs`, in report order: `status`, `method`, `route`, `message`, `problem_type`. Deliberately **no** request ID — it is entropy-minted, and the record has to stay comparable |
| `final_state` | seam totals: `db_checkouts`, `job_executions`, `job_executions_failed`, `job_executions_succeeded` |

`to_json_string()` is canonical (declaration-order fields, no maps, no floats),
`fingerprint()` is an FNV-1a 64 over that string, and `FaultOutcome::from_json_str`
round-trips it. `TestClient::fault_outcome()` is `async` because autumn
dispatches error reports on a detached task: it settles those with bounded
cooperative yields before snapshotting, and never advances the virtual clock
while doing so. `TestClient::fault_ledger()` returns the same handle for an
un-settled `outcome()` snapshot, and `None` when no plan was attached.

#### The determinism contract

The byte-identical-replay guarantee holds inside a specific box, and `build`
enforces the parts it can:

- **A paused, current-thread runtime** — `#[sim_test]`, or
  `#[tokio::test(start_paused = true)]`. On a multi-threaded runtime concurrent
  executions race for their ordinals and the ordering is not reproducible.
- **The injected clock.** Window checks and every `at` / `elapsed_ms` read the
  app's `ClockSource`, so no wall-clock read leaks into the record.
- **Seeded entropy.** Attaching a plan defaults the app's entropy to
  `SeededEntropy` derived from the plan's seed, so retry jitter and minted IDs
  replay from it too. Unlike the rest of the sim this one *is* automatic — an
  explicit `with_entropy` still wins.
- **One job worker** (`jobs.workers = 1`) and **reporting at
  `sample_rate = 1.0`**. `build` asserts both when a plan is attached, rather
  than letting a second worker or a sampled-out 5xx quietly break replay.

#### What it does not cover

- **Only two effect classes**: database connection checkout and job execution.
  Mail, outbound HTTP and channels have no `FaultPlan` lane — for SMTP use
  `Chaos::smtp_faults`, which already takes an explicit 1-based schedule.
- **Test-only.** `FaultPlan` attaches to a `TestApp`; there is no production
  fault injection, by design.
- **`perform_enqueued_jobs` bypasses it.** `TestClient::perform_enqueued_jobs`
  invokes handlers directly and never crosses the `intercept_execute` seam, so
  job faults never fire under it. Drain with `sim.run_to_idle()` — plus
  `sim.advance` to cross a retry backoff — instead.

`autumn/tests/integration/sim_fault_plan.rs` carries the worked example, in the
same before/after shape as the retry-storm bug above: a `charge_card` job and a
plan that fails its first execution. With `max_attempts = 1` the charge is never
recorded and the scenario's resilience assertion fails; with `max_attempts = 3`
the retry lands it exactly once and the same plan passes — flip that one
attribute back to reproduce the failure. See issue
[#1680](https://github.com/autumn-foundation/autumn/issues/1680) for the design.

### `always!` / `sometimes!`

```rust
use autumn_web::{always, sometimes};

always!(total_balance == 0, "ledger must stay zero-sum (seed={:#x})", sim.seed);
sometimes!(response.status() == 409, "a transfer was rejected (insufficient funds)");
```

- `always!` is a **hard invariant** — it panics the instant its condition is
  false, exactly like `assert!`, but the panic flows through `#[sim_test]`'s
  replay-line printing.
- `sometimes!` is a **reachability target** — it records whether the labeled
  condition was ever true. A single run doesn't fail just because a
  `sometimes!` was never satisfied (call
  [`autumn_web::sim::assert_all_sometimes_satisfied`] for an explicit
  single-run check); the seed-sweep runner below fails the *sweep* if a label
  was observed but never satisfied by any seed in the range — so a green sweep
  is provably non-vacuous, never accidentally testing nothing.

### Property-based op-driving + the seed sweep

Behind the `sim-testing` feature (`dep:proptest`), `sim::op` and `sim::sweep`
add a proptest-driven workload generator and a batch seed runner:

```rust
use autumn_web::sim::{Sim, sweep_proptest, SweepOutcome};

let strategy = proptest::collection::vec(any::<Op>(), 1..32);
match sweep_proptest(0..1000, &strategy, |sim, ops| apply_ops(sim, ops)) {
    SweepOutcome::Passed { seeds_run } => println!("{seeds_run} seeds, non-vacuous"),
    SweepOutcome::Failed { failure, .. } => panic!("{failure}"), // shrunk to a minimal op-sequence
    SweepOutcome::Vacuous { unsatisfied, .. } => panic!("never satisfied: {unsatisfied:?}"),
    SweepOutcome::Empty => panic!("swept zero seeds"),
}
```

`sweep_proptest` runs [`Sim::run_proptest`] sequentially across every seed in
the range, stopping at the first failure and reporting its seed plus a
proptest-shrunk minimal op-sequence.

**This is a library function you call from your own `[[bin]]` or test, driven
against your own `Strategy`/`body` for your own app's properties** — nothing
runs it for you automatically. Autumn's own repository ships one example of
wiring it up: `autumn/src/bin/sim_sweep.rs`, a small `[[bin]]` that sweeps a
fixed, deliberately-correct toy account scenario as a smoke check that the
sweep mechanism itself works (`sim_sweep_driver`'s DoD test proves it catches
a real invariant break using a deliberately-buggy variant of the same toy
scenario). Autumn's own CI runs that binary as its own job (`Sim sweep`,
structured like the `loom` job, 512 seeds) on every push and
PR — but that job exercises the harness, not your application. To get this
coverage for your own app, write a `[[bin]]` following the same shape against
your own scenario and wire it into your own CI.

```bash
AUTUMN_SIM_SEEDS=1000 cargo run -p autumn-web --release --features sim-testing --bin sim-sweep
```

---

## Worked example: a real retry-storm bug

The harness's value isn't hypothetical — it caught a genuine bug in autumn's
own job runtime. The local job runtime's retry backoff computed a pure
exponential delay, `initial_backoff_ms * 2^(attempt - 1)`, with **no jitter**.
That delay depends only on a job's *configuration*, not its identity — so when
several jobs in the same queue fail at the same instant (a downstream
dependency blips and takes every in-flight job down with it), every one of
them computes the *identical* delay and retries at the *identical* instant: a
synchronized "thundering herd" that immediately re-floods the dependency it
just backed off from.

A real-clock integration test can't reproduce this on purpose — it would need
N real jobs to fail within the same millisecond of wall time, a coincidence no
ordinary test schedule can force. Under the sim's virtual clock it's the
opposite: "N jobs fail at the same instant" is the deterministic, trivial
condition, because the paused runtime never lets real time elapse between
enqueuing them.

```rust
#[sim_test]
async fn retries_are_not_synchronized_under_load(mut sim: Sim) {
    // The retry jitter reads `state.entropy()` (see "Deterministic
    // identifiers" above), so it needs a seeded source explicitly wired in
    // to actually replay from `AUTUMN_SIM_SEED` — omitting this is a real
    // gap Codex review caught in this test's first draft.
    sim.build(
        TestApp::new()
            .plugin(StormProbeJobPlugin)
            .with_entropy(SeededEntropy::new(sim.seed)),
    );

    for id in 0..STORM_SIZE {
        StormProbeJob::enqueue(StormArgs { id }).await.unwrap();
    }
    sim.run_to_idle().await;                    // every job fails, "at once"

    // Step through the backoff window in bounded checkpoints, recording
    // which checkpoint each retry lands in as it drains. A single big
    // `sim.advance(Duration::from_millis(1_500))` looks tempting here, but
    // both the injected `ClockSource` and Tokio's own paused clock jump
    // straight to their target instant in one step *before* any timer
    // fires — so every retry woken by that one big advance would read the
    // *same* post-advance `now()` regardless of its individual delay,
    // silently hiding a real spread (or a real herd) behind a single
    // observed instant. Checkpointing avoids that: each drain only sees
    // the retries whose delay fell inside that specific step.
    for step_ms in [550, 100, 100, 100, 100, 550] {
        sim.advance(Duration::from_millis(step_ms)).await;
        checkpoint += 1;
        sim.run_to_idle().await;                // records `checkpoint` on each retry that lands here
    }

    let distinct_checkpoints = /* … collected from the drained retries … */;
    always!(
        distinct_checkpoints > 1,
        "all retries landed in the same checkpoint of the backoff window — a thundering herd"
    );
}
```

Before the fix, this `always!` fired on every seed: every retry landed in the
same checkpoint. The fix draws an *equal-jitter* spread — a random delay in
`[ceil(base_delay / 2), base_delay]` — from the framework's injected `Entropy`
seam, so the herd spreads out using real OS entropy in production while
staying bit-for-bit reproducible under a fixed sim seed (the ceiling keeps a
small configured backoff, like `backoff_ms = 1`, from rounding down to an
immediate 0ms retry). See
`autumn/tests/integration/sim_retry_storm.rs` for the full test and
`jittered_retry_delay_ms` in `autumn/src/job.rs` for the fix.

---

## A worked example in an app

`examples/reddit-clone/tests/sim_hot_rank.rs` is the smallest complete shape of
an application `#[sim_test]`: it mounts a route on the sim's paused runtime,
walks 48 virtual hours in checkpoints, and asserts the app's hot-rank decay
curve through the ordinary [`Clock`] extractor rather than around it. It uses
`always!` for the hard invariants and `sometimes!` for reachability, and it
arranges two deliberately-separated input bands so that
[`assert_all_sometimes_satisfied`](autumn_web::sim::assert_all_sometimes_satisfied)
holds at every seed — the pattern to copy when you want a single-run
non-vacuity check rather than a sweep.

## What's virtualized (and what isn't)

| Source | Sim treatment |
|---|---|
| Wall-clock time (`Utc::now()` via the `Clock` extractor) | Virtual, driven by `Sim::advance` |
| Async timers (`tokio::time::sleep`, job backoff, scheduler ticks) | Virtual, via a paused current-thread Tokio runtime |
| Elapsed / monotonic time (`state.monotonic()`, the `Clock` extractor's `.monotonic()`) | Virtual, driven by `Sim::advance` — but a raw `std::time::Instant` is **not** (see below) |
| Scheduling of autumn's own background work (jobs, scheduler, commit hooks) | Deterministic, drained by `Sim::run_to_idle` |
| Framework-minted IDs (job IDs, request IDs, idempotency keys, sessions) | Seeded via the `Entropy` seam |
| Database | **Boundary** — real in-process SQLite, fault-injected at the connection level via `Chaos` (by probability) or `FaultPlan` (by checkout ordinal), not simulated at the SQL-dialect level |
| Third-party network (SMTP, LLM calls, outbound HTTP) | **Boundary** — mocked/fault-injected via `Chaos`/`sim::llm`, not a full network simulator |

### Keeping your own code deterministic

Tokio's paused runtime virtualizes `tokio::time::Instant` — **not**
`std::time::Instant`. So a handler or job that measures how long something took
with `std::time::Instant::now()` reads the real machine clock even inside a
`#[sim_test]`, and two runs of the same seed disagree. Read time through the
seams instead:

| Instead of | Use |
|---|---|
| `chrono::Utc::now()` | `state.clock().now()`, or the `Clock` extractor in a handler |
| `std::time::Instant::now()` (measuring elapsed) | `clock.monotonic()` for the start (the extractor snapshots at request start) and `state.monotonic()` for the closing read, then `MonotonicInstant::saturating_duration_since` |
| `std::time::Instant::now()` (a deadline whose counterparty is `tokio::time::sleep`) | `tokio::time::Instant::now()` — already virtual under the paused runtime |
| `std::time::SystemTime::now()` | `autumn_web::time::clock_unix_secs(clock)` / `clock_unix_duration(clock)` |
| `uuid::Uuid::new_v4()` | `state.entropy().uuid_v4()`, or the `Rng` extractor in a handler |

If you write a custom `impl ClockSource` whose `now()` is virtual, you **must**
also override `monotonic()`. The trait ships a default body that reads the real
process-monotonic clock — that keeps every pre-existing implementation compiling
and behaving exactly as before, but it means a virtual clock that forgets to
override it silently reports real elapsed time.

The framework holds itself to the same rule on the modules listed in
`scripts/check-determinism-gate.sh`, where a clippy deny-lint enforces it; see
CONTRIBUTING.md's "Determinism seam gate" for the enforced subset and the parts
of the framework that are not on the seam yet.

A sim-only green run proves your orchestration, timing, ordering, and identity
logic — it is **not** a substitute for testcontainer integration tests against
real Postgres, and it does not simulate SQL-dialect or isolation-level
behavior. See issue [#1797](https://github.com/autumn-foundation/autumn/issues/1797)
for the full design rationale and determinism boundary.
