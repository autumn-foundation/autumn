# Simulation Testing

Autumn ships a **deterministic simulation testing (DST)** harness: a `#[sim_test]`
attribute that hands your test a seeded [`Sim`] handle, a paused deterministic
executor, and a virtual clock, so your whole app's concurrency — retries,
scheduled ticks, background jobs — runs identically on every machine and every
run. A failure prints a copy-pasteable line that reproduces it exactly, and a
seed-sweep runner explores thousands of seeds per commit looking for
rare-interleaving bugs a conventional integration test would never stumble
into.

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
    // Everything derived from `sim` — its clock, its RNG, the app it mounts —
    // is a pure function of this seed.
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
sim's paused runtime, wired to the virtual clock, seeded entropy, and (if
configured) the fault-injection hooks below.

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
durable-recovery proofs).

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
proptest-shrunk minimal op-sequence. The CI-facing driver is the `sim-sweep`
binary:

```bash
AUTUMN_SIM_SEEDS=1000 cargo run -p autumn-web --release --features sim-testing --bin sim-sweep
```

This runs as its own CI job (`Sim sweep`, structured like the `loom` job) on
every push and PR.

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

## What's virtualized (and what isn't)

| Source | Sim treatment |
|---|---|
| Wall-clock time (`Utc::now()` via the `Clock` extractor) | Virtual, driven by `Sim::advance` |
| Async timers (`tokio::time::sleep`, job backoff, scheduler ticks) | Virtual, via a paused current-thread Tokio runtime |
| Scheduling of autumn's own background work (jobs, scheduler, commit hooks) | Deterministic, drained by `Sim::run_to_idle` |
| Framework-minted IDs (job IDs, request IDs, idempotency keys, sessions) | Seeded via the `Entropy` seam |
| Database | **Boundary** — real in-process SQLite, fault-injected at the connection level via `Chaos`, not simulated at the SQL-dialect level |
| Third-party network (SMTP, LLM calls, outbound HTTP) | **Boundary** — mocked/fault-injected via `Chaos`/`sim::llm`, not a full network simulator |

A sim-only green run proves your orchestration, timing, ordering, and identity
logic — it is **not** a substitute for testcontainer integration tests against
real Postgres, and it does not simulate SQL-dialect or isolation-level
behavior. See issue [#1797](https://github.com/autumn-foundation/autumn/issues/1797)
for the full design rationale and determinism boundary.
