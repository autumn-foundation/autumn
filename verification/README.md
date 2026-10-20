# Verification

This directory contains small mathematical shadows of critical runtime state.
It also contains the protocol models in `models/` (see "Protocol models").
The Verus specs are not in the Cargo workspace, because Verus uses an extended
Rust dialect. Verify the tenant arena spine with:

```sh
verus verification/tenant_arena.rs
```

The durable job claim lease (issue #3051, ADR 0016) has its own model:

```sh
verus verification/job_lease.rs
```

The runtime correspondence and boundary are recorded in ADR 0012; executable
tests remain authoritative for unmodeled allocator, HTTP, and concurrency glue.

## CI

`.github/workflows/verus.yml` runs `scripts/verify-verus.sh`, which runs Verus
on every `verification/*.rs` file. A new spec needs no workflow change. The job
runs on a change to this directory, to the script, or to the workflow. It also
runs each Monday. It is not a required check yet, so a failed proof does not
block a merge. Do not add it to branch protection until it is stable.

Run the same check on your machine:

```sh
scripts/verify-verus.sh                          # uses `verus` on PATH
VERUS_BIN=/path/to/verus scripts/verify-verus.sh
```

## Lease-lock fencing

`lease_fencing.rs` models the `autumn_lease_locks` statements in
`autumn/src/lock/lease.rs` (issue #3053, ADR 0015). Verify it with:

```sh
verus verification/lease_fencing.rs
```

It proves that tokens are unique and strictly increasing per lock name, and
that a stale write is rejected. It does not prove that holders never overlap;
the fencing token makes overlap safe.

## Fleet slot ownership

`fleet_slot_ownership.rs` models the slot-ownership table of a SQLite fleet
(`OwnershipTable` in `autumn/src/db/fleet_ownership.rs`, issue #3233). Verify
it with:

```sh
verus verification/fleet_slot_ownership.rs
```

It proves that epochs are unique and strictly increasing, that a slot's epoch
never goes down, that a live lease of another owner blocks a claim, and that
the old owner cannot renew, release or write after a newer claim. It also
proves that a restore never takes a generation of an older epoch, and that a
fenced replicator stops. The proptest `the_table_keeps_its_invariants` checks
the same invariants on the runtime table.

## Billing plan gate

`billing_gate.rs` models the plan gate selection in
`autumn-billing/src/gate.rs` (issue #3114). Verify it with:

```sh
verus verification/billing_gate.rs
```

The spec proves the selection contract:

- When no entitled view satisfies the rule, the gate denies.
- When at least one view satisfies the rule, the gate selects a view.
- The selected view satisfies the rule and comes first in gate order. Gate
  order is the latest `last_event_at` first, then the lowest `id`.

The model does not include plan lookup, status rules or the grace period.
The integration test `gate_matches_the_any_row_oracle_for_every_pair_of_rows`
covers those parts with the default config. The test
`require_breaks_an_event_time_tie_by_row_id` covers the tie on `id`.

## Deploy bake verdict

`bake_verdict.rs` models `judge` in `autumn-cli/src/deploy/bake.rs`
(issue #3069, ADR 0017). Verify it with:

```sh
verus verification/bake_verdict.rs
```

It proves five rules:

- Thin traffic never rolls back.
- A restart always rolls back.
- Fewer than `min_errors` new 5xx never give an error breach.
- More errors cannot change an error breach to a pass.
- A pass means that no limit is exceeded.

Verus checks the executable `judge` in the file against the spec, with the
same `u128` arithmetic as the runtime. The runtime `judge` is separate code.
The property test `judge_matches_the_verus_model` checks it against a Rust
copy of `spec_judge`.

## Admission partitions

`admission_partitions.rs` models the criticality thresholds in
`autumn/src/admission.rs` and the admission check in
`autumn/src/middleware/load_shed.rs` (issue #3068, ADR 0016). Verify it with:

```sh
verus verification/admission_partitions.rs
```

It proves that the thresholds are nested (`sheddable <= default <= critical =
limit`). Thus, if the server rejects a higher class, it also rejects every
lower class, and no class is admitted at or above the limit. It also proves
that the `u128` threshold product cannot overflow, and that the AIMD update
keeps the limit in `min..=max`. The AIMD model rounds the back-off up; the
runtime rounds it down. The clamp makes the property true for both. The model
does not include the floating-point algorithms. Unit tests and sim tests
examine them.

## Circuit breaker half-open accounting

`circuit_breaker.rs` models `CircuitBreakerInner` in
`autumn/src/circuit_breaker.rs`. Each step is one critical section of the
runtime. It proves:

- Every step keeps the invariant: in the half-open state, the successful
  trials plus the running trials never exceed `half_open_trial_count`.
- A fresh half-open breaker admits a trial. This needs at least one trial.
  With 0 trials, the breaker refuses every call for ever, and the proof
  `zero_trials_strand_the_breaker` shows it. That found a real bug: a policy
  built in code skipped the clamp to 1. `CircuitBreakerPolicy::clamped` now
  applies it.
- A call from an older generation changes nothing, and the generation never
  goes back.

## Retry budget

`retry_budget.rs` models the token bucket in `autumn/src/retry_budget.rs`
(issue #3058). It proves that a step never passes the capacity, that a
release undoes its acquire, and that a full bucket grants exactly
`capacity / cost` retries. Shuttle and loom check the compare-exchange loop
around each step (see below).

## Tenant bulkhead

`bulkhead.rs` models `TenantBulkhead` in `autumn/src/bulkhead.rs`. It proves
that no tenant passes its cap, that tenants are isolated, and that a permit
that comes back leaves the map exactly as it was. Thus the map cannot leak
entries for tenant ids from requests.

## Backoff and jitter

`backoff.rs` models `ceiling_ms` and `full_jitter_ms` in
`autumn/src/backoff.rs` (issue #3054), and the equal jitter of the outbox
relay. Its `ceiling_ms` uses the runtime's shift and saturating multiply, and
Verus proves that it equals `min(cap, base * 2^retry)`. It also proves that
the ceiling never decreases, and that no jitter overflows.

## Kani proofs

Kani checks the shipped functions, not a model of them. The harnesses are in
`#[cfg(kani)] mod kani_proofs` blocks next to the code:

| File | Proves |
| --- | --- |
| `autumn/src/backoff.rs` | The ceiling is capped, exact and monotonic. Every jitter draw is in `[0, ceiling]`. A `Retry-After` wait is in its window. |
| `autumn/src/admission.rs` | The `u128` scale is the exact floor for every `usize` limit, so with `scaled_is_monotone` above the thresholds are nested for every limit. No threshold is above the limit, and a positive share never sheds at idle. An AIMD step stays in bounds. The `f64` clamp maps NaN and both infinities into bounds. |
| `autumn/src/range.rs` | A satisfiable range is inside the body, for every short spec and every size. The slicer never panics. |
| `autumn/src/outbox.rs` | The relay retry delay is in `[ceiling / 2, ceiling]` and never overflows. |
| `autumn/src/custom_domain.rs` | The certificate backoff is capped, monotonic, and starts at the base. |
| `autumn/src/time_math.rs` | `saturating_time_delta_secs` never panics, and is exact when the count fits. |

Run them with:

```sh
cargo install --locked kani-verifier --version 0.68.0
cargo kani setup
cargo kani -p autumn-web --lib
```

`.github/workflows/kani.yml` runs them when a file with a harness changes, and
each Monday. It is not a required check yet. The workflow names each file that
holds a harness. `resilience_hygiene.rs` fails when a harness is in a file that
it does not name.

## loom and Shuttle over the real code

`autumn/src/sync_shim.rs` gives the retry budget, the tenant bulkhead, the
circuit breaker and the adaptive limiter their locks and atomics. In a normal
build they are `std::sync`. Under `--cfg loom_models` they are loom's, and
under `--cfg shuttle_models` they are Shuttle's. Thus the `real_code` models
in `autumn/tests/loom_models.rs` and every test in
`autumn/tests/shuttle_models.rs` run the shipped code:

```sh
RUSTFLAGS="--cfg loom_models" LOOM_MAX_PREEMPTIONS=3 \
  cargo test -p autumn-web --release --test loom_models
RUSTFLAGS="--cfg shuttle_models" \
  cargo test -p autumn-web --release --test shuttle_models
```

loom checks every interleaving of two or three threads. Shuttle samples random
and PCT schedules, so it checks more threads and async tasks that hold a lock
or a permit across an `.await`. Its test `shuttle_finds_a_seeded_lost_update`
must find a lost update in a racy counter. If it does not, the scheduler is
not interleaving and the other tests prove nothing.

A module can use the shim only when it creates every lock and atomic at run
time. A `static` is created outside the checker, and loom and Shuttle types
panic there.

## Protocol models

`models/` is the `autumn-protocol-models` crate (issue #3071). It holds
[Stateright](https://www.stateright.rs) models of three protocols:

| Model | Production code | Properties |
| --- | --- | --- |
| `job_claim` | `autumn/src/job.rs` (`docs/adr/0016-durable-job-claim-lease.md`) | A stale holder's settle is rejected. At most one execution runs at a time. |
| `tick_election` | `autumn/src/scheduler.rs`, `execute_cron_task` (#3052) | Each tick runs at most once. |
| `lease_lock` | `autumn/src/lock/lease.rs` (`docs/adr/0015-fencing-lease-lock.md`) | A stale holder's write is rejected. Tokens are unique per grant. At most one holder trusts its lease. |

The Verus specs above prove one step at a time. The models check every
interleaving of workers, replicas, late messages and clock steps, within small
bounds. Run them with:

```sh
cargo test -p autumn-protocol-models
```

Each model has a `Variant`. `Correct` must hold every `always` property and
reach every `sometimes` property. Each seeded bug (for example
`SettleWithoutOwnerFence`) must give a counterexample. If a seeded bug gives
no counterexample, its test fails.

The tick model found a bug: a cron task that waited in the cost gate past its
window could claim a pruned tick and run it twice. `execute_cron_task` now
checks the window after the wait. The seeded bug `NoLatenessCheck` keeps that
path.

When a protocol changes, change its model in the same PR. `cargo test
--workspace` runs the models. The `Protocol models` job in `ci.yml` also runs
them.
