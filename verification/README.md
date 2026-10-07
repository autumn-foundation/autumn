# Verus specifications

This directory contains small mathematical shadows of critical runtime state.
They are intentionally separate from the Cargo workspace because Verus uses an
extended Rust dialect. Verify the tenant arena spine with:

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
