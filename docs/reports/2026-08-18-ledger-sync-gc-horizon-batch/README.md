# 🗃️ Ledger: batch PgSyncBackend::gc_tombstones horizon UPSERTs (statements/sweep N→1)

## 🎯 Workload

`PgSyncBackend::gc_tombstones` (`autumn/src/sync/server.rs`) is the offline-sync
tombstone GC: a maintenance sweep an operator calls deliberately (e.g. from a
scheduled task, per the trait docs) once all active devices are expected to
have synced past a version. It physically drops tombstone rows across **ALL
scopes** with `version <= up_to`, then advances **each affected scope's**
tombstone horizon — tracked per scope so one tenant's GC never forces another
tenant's full resync (see the `SyncBackend` trait docs at
`autumn/src/sync/server.rs:540-559`).

The drop itself is already one batched `DELETE ... RETURNING scope, version`.
But the horizon advance that follows it was a **loop**:

```rust
let mut per_scope: HashMap<String, Version> = HashMap::new();
for record in dropped {
    let max = per_scope.entry(record.scope).or_insert(0);
    *max = (*max).max(record.version);
}
for (scope, horizon) in per_scope {
    sql_query(
        "INSERT INTO autumn_sync_horizons (scope, horizon) VALUES ($1, $2) \
         ON CONFLICT (scope) DO UPDATE SET \
         horizon = GREATEST(autumn_sync_horizons.horizon, excluded.horizon)",
    )
    .bind::<Text, _>(&scope)
    .bind::<BigInt, _>(horizon)
    .execute(conn)?;
}
```

One round trip per **distinct scope** the sweep touched, all inside the same
transaction that holds `pg_advisory_xact_lock` (the same lock `apply_push`
takes, serializing GC against in-flight pushes — see the code comment at
`server.rs:1494-1501`). In a single-tenant deployment this is invisible: one
scope, one extra statement. In a multi-tenant deployment — the case the trait
docs explicitly design the per-scope horizon for — a GC sweep can cover
hundreds or thousands of tenant scopes in one call, and the lock is held for
all of it.

This is the same shape as the already-fixed `apply_push` push-batching commit
(#2225): a maintenance/hot-path function in `PgSyncBackend` where an
otherwise-cheap per-row round trip compounds with N. The commit-hooks bulk
stager (`repository_commit_hooks.rs::enqueue_repository_commit_hooks_pending_bulk_on_conn`)
already establishes the exact batching pattern used here — a Postgres
`UNNEST($1::TYPE[], ...)` multi-row `INSERT ... ON CONFLICT DO UPDATE` in one
statement.

**Fixture**: a multi-tenant offline-sync deployment. `autumn_sync_rows` seeded
across three disjoint scope tiers — 50 / 250 / 1000 distinct scopes, the same
batch sizes `offline_sync_push_batching_perf.rs` uses — 20 rows per scope,
30% tombstoned (deterministic, no flaky row-count-dependent skips), real dead
tuples from a follow-up `UPDATE`, `ANALYZE`d. One scope (`tenant-000000`) is
pre-seeded with an existing horizon (`999999`) higher than anything the sweep
computes, to pin the `GREATEST` no-lower-horizon guarantee through the
rewrite.

**Reproduce**:
```bash
cargo test -p autumn-web --features "test-support,offline-sync" \
  --test integration_tests -- --ignored offline_sync_gc_tombstones_batching_profile \
  --nocapture --test-threads=1
```
Requires Docker (spins up a `postgres:16-alpine` testcontainer with
`pg_stat_statements` preloaded). CI runs it automatically in the
Docker-dependent `#[ignore]` sweep (see `CLAUDE.md`).

## 📈 Profile

Within this one-sweep workload, two statements dominate: the tombstone
`DELETE ... RETURNING` (unchanged by this fix) and the horizon-upsert
statement(s) (the fix). At the 1000-scope tier, before the fix:

| statement | calls | buffers | share of sweep's statement calls |
|---|---:|---:|---:|
| horizon UPSERT (per-scope) | 1000 | 6,029 | 99.9% |
| tombstone `DELETE ... RETURNING` | 1 | 12,506 | 0.1% |

The horizon upsert is *not* the dominant buffer cost (the single batched
`DELETE` reading/writing 20,000 rows costs more per call) — it dominates
`calls`, which is exactly the ranking the Ledger process calls out as where
N+1s hide ("this is where N+1 lives, and it is usually invisible in the
buffer ranking because each individual statement is cheap"). This is an
N+1-elimination claim, admissible on its own per the impact floor, not a
buffers-share claim.

## 🧭 Plan

Same access method either side — a primary-key (`scope`) index upsert — this
is not a plan-shape change, it's the same operation issued once instead of N
times. Representative `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` (full
output in `baseline/output.txt` / `after/output.txt`):

**Before** (single-row form, issued once per scope):
```
Insert on public.autumn_sync_horizons  (cost=0.00..0.01 rows=0 width=0) (actual rows=0 loops=1)
  Conflict Resolution: UPDATE
  Conflict Arbiter Indexes: autumn_sync_horizons_pkey
  Buffers: shared hit=11
```

**After** (UNNEST-batched form, illustrative 20-scope sample — the full
50/250/1000-scope scale claim is the 📊 Measurement table below, from
`pg_stat_statements`, not this diagnostic EXPLAIN):
```
Insert on public.autumn_sync_horizons  (cost=0.01..10.01 rows=0 width=0)
  Conflict Resolution: UPDATE
  Conflict Arbiter Indexes: autumn_sync_horizons_pkey
  ->  Function Scan on t  (cost=0.01..10.01 rows=1000 width=40)
        Function Call: unnest(...)
```
One `Function Scan on t` (the `UNNEST` array-to-rows expansion) feeding one
`Insert` node with the identical conflict-arbiter index and `GREATEST`
resolution — same mechanism, N inputs processed by one executor invocation
instead of N separate ones.

## 💡 Hypothesis

"`gc_tombstones` issues one horizon-upsert statement per distinct scope
touched by the sweep, inside a lock-held transaction, because the per-scope
max-version computation (`HashMap<String, Version>`) is drained with a Rust
`for` loop that calls `.execute()` per entry instead of binding the whole map
as arrays to a single `UNNEST`-driven statement." The mechanism is
structural — no missing index, no bad plan — Postgres already picks the
optimal single-row index upsert path every time; the defect is that the code
asks it to do that N times instead of once.

## 🔧 Change

One change, `autumn/src/sync/server.rs`, `PgSyncBackend::gc_tombstones`: the
`for (scope, horizon) in per_scope { sql_query(...).execute(conn)?; }` loop
becomes

```rust
if !per_scope.is_empty() {
    let (scopes, horizons): (Vec<String>, Vec<Version>) = per_scope.into_iter().unzip();
    sql_query(
        "INSERT INTO autumn_sync_horizons (scope, horizon) \
         SELECT * FROM UNNEST($1::TEXT[], $2::BIGINT[]) AS t(scope, horizon) \
         ON CONFLICT (scope) DO UPDATE SET \
         horizon = GREATEST(autumn_sync_horizons.horizon, excluded.horizon)",
    )
    .bind::<Array<Text>, _>(scopes)
    .bind::<Array<BigInt>, _>(horizons)
    .execute(conn)?;
}
```

Same `ON CONFLICT ... DO UPDATE SET horizon = GREATEST(...)` clause, same
conflict arbiter (`autumn_sync_horizons_pkey`), same advisory-lock-held
transaction — Postgres expands `UNNEST` server-side into the same set of rows
the loop would have inserted one at a time, and applies the conflict rule to
each independently, so this is a batching change, not a semantics change. The
empty-sweep guard (`if !per_scope.is_empty()`) avoids issuing a
zero-row-array statement when a sweep drops no tombstones (previously the
loop simply didn't iterate; `UNNEST` on two empty arrays is technically valid
SQL but pointless to issue).

No migration, no new index, no Diesel schema change, no change to
`gc_applied` or any other `SyncBackend` method.

## 📊 Measurement

pg_stat_statements, one `gc_tombstones` sweep per tier (fresh
`pg_stat_statements_reset()` before each):

| scopes touched | before calls | after calls | before buffers | after buffers | before WAL bytes | after WAL bytes |
|---:|---:|---:|---:|---:|---:|---:|
| 50   | 50   | **1** | 207  | 207  | 9,873   | 9,873   |
| 250  | 250  | **1** | 1,027 | 1,027 | 50,664  | 50,664  |
| 1000 | 1000 | **1** | 6,029 | 6,029 | 210,808 | 210,808 |

Statement count drops from exactly `N` (one per distinct scope) to exactly
`1`, at every tier — the classic N+1 shape, confirmed at three sizes to rule
out a fixture-specific coincidence. Buffers and WAL bytes are unchanged
(same rows read/written either way, same total work — just one executor
invocation instead of N). Idempotency: a replay sweep at the same `up_to`
(nothing left to drop) issues **zero** horizon-upsert statements, both
before and after.

This clears the impact floor on **elimination of an N+1**
(`statements/sweep 1000→1` at the largest tier), which "needs no other
justification" per the process — no buffers/rows-read claim is needed
alongside it, though buffers are reported above for completeness (unchanged,
as expected for a pure batching change).

## ✅ Equivalence

The full `autumn_sync_horizons` table, dumped `ORDER BY scope` after all
three tiers' sweeps plus the idempotency replay, is **byte-identical**
between the baseline and after runs — all 1,300 seeded scopes
(`baseline/output.txt` vs. `after/output.txt`, `HorizonDump { ... }` lines).
Both runs are fully deterministic (fixed fixture, fixed `up_to` boundaries,
no wall-clock, no randomness), so a byte-identical dump is a real proof, not
a coincidence of one run.

Edge cases exercised:
- **`GREATEST` no-lower-horizon guarantee**: `tenant-000000` is pre-seeded
  with horizon `999999`, well above anything tier 1's sweep computes for
  that scope. Asserted equal to `999999` after the batched UPSERT in both
  runs — a pre-existing higher horizon is never overwritten by a lower
  computed one, whether the UPSERT arrives as N single-row statements or one
  batched statement.
- **Empty sweep**: the idempotency replay (`gc_tombstones` at the same
  `up_to` a second time) drops zero tombstones, so `per_scope` is empty and
  the guarded `UNNEST` statement is skipped entirely — asserted as `calls ==
  0` for the horizon-upsert statement, both before and after.
- **Duplicate scope keys**: `per_scope` is a `HashMap<String, Version>`
  built by `record.scope` keys before either code path runs, so the same
  scope with multiple dropped tombstones was already deduplicated
  (max-version-wins) prior to this change; the `UNNEST` arrays therefore
  never contain a repeated scope, so no double-`ON CONFLICT` fan-out risk
  was introduced — the array-building step (`per_scope.into_iter().unzip()`)
  is a direct drain of the same deduplicated map the loop used.
- Isolation/visibility unchanged: both forms run inside the same
  `conn.transaction(...)` block under the same `pg_advisory_xact_lock`, and
  no other statement moved in or out of that transaction.

Bi-temporal / as-of semantics: not applicable — tombstone horizons are a
scalar-per-scope watermark, not a validity interval; this change doesn't
touch `pull_since`'s horizon-comparison logic, only how the watermark is
persisted.

Existing tests pass **unchanged**:
`offline_sync_pg::pg_backend_passes_conformance` (the shared conformance
suite, which exercises `gc_tombstones` semantics including horizon
comparisons on `pull_since`) and the rest of the `offline_sync_*`/`pg_tls`
suite — 39 passed, 0 failed, plus the conformance test run explicitly
(`--ignored pg_backend_passes_conformance`) — all pass with no expectation
changes.

## 💸 Write cost

None beyond the sweep's own writes (which are unchanged — see 📊
Measurement: WAL bytes identical before/after). No index was added or
dropped; `autumn_sync_horizons_pkey` (the existing primary key on `scope`)
already backs both the single-row and batched conflict-arbiter path. This is
a pure statement-batching change on an existing write path, not a new write
path.

## 🔬 Reproduce

```bash
# Full harness (spins up its own postgres:16-alpine testcontainer):
cargo test -p autumn-web --features "test-support,offline-sync" \
  --test integration_tests -- --ignored offline_sync_gc_tombstones_batching_profile \
  --nocapture --test-threads=1

# Conformance + full offline-sync suite (regression check):
cargo test -p autumn-web --features "test-support,offline-sync" \
  --test integration_tests -- --ignored pg_backend_passes_conformance --nocapture --test-threads=1
cargo test -p autumn-web --features "test-support,offline-sync" \
  --test integration_tests offline_sync -- --skip batching_profile
```

`baseline/output.txt` is this harness's stdout on the pre-fix code (commit
adding the harness); `after/output.txt` is the same command's stdout after
the batching change, same fixture, same session — both committed in full.
