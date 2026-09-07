# 🗃️ Ledger: batch ExperimentAdminModel bulk delete (statements/action 615→1)

## 🎯 Workload

`POST /admin/{slug}/actions` (`autumn-admin-plugin/src/routes.rs`,
`model_action`) is the admin panel's bulk-action endpoint: it parses a form
body carrying `action=<name>` plus a **repeated, uncapped** `ids=<id>` field
(one entry per row an operator selected in the list view), then calls
`model.execute_action(&pool, &action, ids)`.

[`AdminModel::execute_action`](../../../autumn-admin-plugin/src/traits.rs)
(the default every model gets unless it overrides the method) dispatches
`"delete"`/`"restore"`/`"purge"` by looping over `ids` and calling the
model's single-row `delete`/`restore`/`purge` once per id:

```rust
"delete" => {
    let mut count: u64 = 0;
    for id in ids {
        self.delete(&pool, id).await?;
        count += 1;
    }
    Ok(count)
}
```

[`ExperimentAdminModel`](../../../autumn-admin-plugin/src/experiments.rs)
(the built-in admin model for A/B experiments, `/admin/experiments/`)
didn't override `execute_action`, so it inherited this loop. Its `delete()`
is a `pool.get()` + single-row CTE round trip
(`DELETE FROM autumn_experiments WHERE id = $1 RETURNING name`, cascading
to that experiment's sticky assignments and staff overrides, and feeding
an audit `INSERT` into `autumn_experiment_changes`) — so an operator
selecting hundreds of concluded/archived experiments in a multi-year-old
app's cleanup and clicking "Delete selected" cost one statement, and one
connection checkout, **per experiment**, not per click. This is the same
shape already closed for `TokenAdminModel`
(`docs/reports/2026-08-31-ledger-admin-bulk-delete-batch/`) and
`FeatureFlagAdminModel`
(`docs/reports/2026-09-06-ledger-feature-flag-admin-bulk-delete-batch/`);
`ExperimentAdminModel` was the next model left on the trait default.

**Fixture**: a 3,000-row `autumn_experiments` table (the real schema from
`autumn/migrations/20260530300000_create_experiments/up.sql`, included
verbatim via `include_str!` so the fixture can't drift from what the admin
UI actually manages) — a plausible size for a long-lived app that never
prunes concluded/archived experiments. 35% NULL `description`, state
skewed toward `concluded` (50%) and `archived` (25%) over `running` (15%)
and `draft` (10%) — the long-tail shape of an app that ships new
experiments continuously but rarely deletes old ones — `winner` set on
~71.4% of concluded rows (measured and printed by the harness itself, not
assumed) and NULL elsewhere, and `exclusion_group` NULL for 60% of rows,
else one of 15 group names (cardinality skew).

Each experiment carries a variable number of sticky assignments
(`autumn_experiment_assignments`, 10–170 rows per experiment depending on
how wide its rollout was, ~270k rows total) and 0–3 staff overrides
(`autumn_experiment_overrides`, ~4.5k rows total) — the two cascading child
tables `delete()`'s CTE also has to touch, real cardinality skew rather
than a uniform fixture. `autumn_experiment_changes` (the audit log the real
`delete()` CTE writes to) is pre-seeded with 3 rows per experiment (9,000
rows) so the audit table is a realistic size, not empty, when the bulk
action adds to it. Real dead tuples come from a follow-up `UPDATE` before
`ANALYZE`.

The bulk-delete selection is 600 ids — a plausible one-shift "prune every
experiment this quarter's cleanup marked dead" operator action — scattered
every 5th id across the table (not a contiguous head block), of which 45
are force-deleted *before* the action runs (a narrower cleanup already
caught them — must stay a no-op, not error), plus 15 ids past the table's
range that never existed at all (same requirement). The exact pre-existing
count (555) is measured with a `COUNT(*)`, not assumed, per the lesson
called out in the `TokenAdminModel`/`FeatureFlagAdminModel` harnesses this
one mirrors.

**Reproduce**:
```bash
cargo test -p autumn-admin-plugin --test experiment_admin_bulk_delete_batch_profile \
  -- --ignored --nocapture --test-threads=1
```
Requires Docker (spins up a `postgres:16-alpine` testcontainer with
`pg_stat_statements` preloaded). This crate has no consolidated
`tests/integration/mod.rs` (unlike `autumn`/`autumn-cli`), and CI does not
run a bare `--ignored` sweep over this package either — this binary is
invoked by an explicit `--test experiment_admin_bulk_delete_batch_profile`
line in `.github/workflows/ci.yml`, next to the existing `token_admin_*` /
`feature_flag_admin_*` lines (both in the "Run Docker-dependent tests" step
and the coverage step) — a bare sweep would silently never compile or run
it.

## 📈 Profile

This harness drives exactly one workload — the bulk-delete action — so
there's no cross-statement ranking to build: the delete CTE (whichever
shape it takes) is the entire measured cost. It is not a small slice of a
bigger request; it **is** the request. The relevant "profile" here is the
`calls` count against a single, well-known statement shape — exactly the
signal the Ledger process calls out as invisible in a buffer ranking but
dominant in `pg_stat_statements.calls`: "individually trivial, collectively
dominant."

## 🧭 Plan

Same access method either side — a primary-key point/array lookup via
`autumn_experiments_pkey`, cascading through an index scan on
`idx_autumn_exp_assignments_experiment`/`idx_autumn_exp_overrides_experiment`,
no seq scan, no sort:

**Before** (`baseline/output.txt`, `calls=615`):
```
WITH deleted AS ( DELETE FROM autumn_experiments WHERE id = $1 RETURNING name ), _del_assignments AS ( DELETE FROM autumn_experiment_assignments WHERE experiment IN (SELECT name FROM deleted) ), _del_overrides AS ( DELETE FROM autumn_experiment_overrides WHERE experiment IN (SELECT name FROM deleted) ), _audit AS ( INSERT INTO autumn_experiment_changes (experiment, mutation, actor) SELECT name, $2, $3 FROM deleted ) SELECT COUNT(*) AS count FROM deleted
```

**After** (`after/output.txt`, `calls=1`):
```
WITH deleted AS ( DELETE FROM autumn_experiments WHERE id = ANY($1) RETURNING name ), _del_assignments AS ( DELETE FROM autumn_experiment_assignments WHERE experiment IN (SELECT name FROM deleted) ), _del_overrides AS ( DELETE FROM autumn_experiment_overrides WHERE experiment IN (SELECT name FROM deleted) ), _audit AS ( INSERT INTO autumn_experiment_changes (experiment, mutation, actor) SELECT name, $2, $3 FROM deleted ) SELECT COUNT(*) AS count FROM deleted
```

Every id-scoped delete CTE, its cascading assignment/override deletes, the
audit `INSERT` they trigger, and the loop's own `pool.get()` call collapse
into one round trip carrying one bound `bigint[]` array instead of 615
separately-prepared, separately-executed statements. Both dumps end with
two diagnostic `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` runs, each
rolled back: the pre-fix single-id shape (`id = 2`) and — so the "same plan
either side" claim rests on the statement `execute_action` actually issues
post-fix, not just on analogy with the single-id case — the real post-fix
batched shape (`id = ANY(ARRAY[2,3,4,6,7])`, a representative array of
surviving ids). Both show the identical per-row plan (index scan on
`autumn_experiments_pkey`, bitmap-index-scan-driven cascade deletes on both
child tables, CTE-scan-driven audit insert, the
`autumn_experiment_change_notify` trigger firing once per deleted row) —
this is a round-trip-count change, not a plan-shape change.

## 💡 Hypothesis

"`execute_action`'s default `"delete"` branch is a
`for id in ids { self.delete(&pool, id).await?; }` loop (traits.rs) — one
DB round trip *and* one connection checkout per id.
`ExperimentAdminModel::delete` (experiments.rs) is a single-statement CTE:
`DELETE FROM autumn_experiments WHERE id = $1 RETURNING name`, cascading to
`autumn_experiment_assignments`/`autumn_experiment_overrides` keyed on the
returned `name`, then feeding
`INSERT INTO autumn_experiment_changes SELECT name, 'deleted', NULL FROM deleted`.
The fix is mechanical: override `execute_action` on `ExperimentAdminModel`
for the `"delete"` action to widen the predicate to `id = ANY($1)` over the
whole id list — the cascading deletes and the audit `INSERT`'s
`SELECT ... FROM deleted` already fan out to one row per id the `DELETE`
actually removed, so no other clause needs to change."

## 🔧 Change

`autumn-admin-plugin/src/experiments.rs`: `ExperimentAdminModel` now
overrides `execute_action`. The `"delete"` branch issues one
`WITH deleted AS (DELETE FROM autumn_experiments WHERE id = ANY($1) RETURNING name), _del_assignments AS (DELETE FROM autumn_experiment_assignments WHERE experiment IN (SELECT name FROM deleted)), _del_overrides AS (DELETE FROM autumn_experiment_overrides WHERE experiment IN (SELECT name FROM deleted)), _audit AS (INSERT INTO autumn_experiment_changes (experiment, mutation, actor) SELECT name, 'deleted', NULL FROM deleted) SELECT COUNT(*) AS count FROM deleted`
bound to the full `ids: Vec<i64>`, and returns `ids.len()` — matching the
loop's own counting behavior exactly (see Equivalence). `ExperimentAdminModel`
never declares soft delete (`supports_soft_delete()` is the trait default,
`false`), so `actions()` only ever offers `"delete"`; the `"restore"`/
`"purge"`/unhandled-action branches are kept as unchanged copies of the
trait default's per-id loop, purely so a direct or out-of-band call to
`execute_action("restore", …)`/`execute_action("purge", …)` still gets the
exact same "does not support soft delete" error it always did. There is no
batching concern there: `self.restore`/`self.purge` are the trait's default
methods, which return `Err` on the very first id regardless of loop shape.

No migration — the tables and their indexes are unchanged; this only
changes how many round trips one bulk action costs. This is a single,
scoped override (`ExperimentAdminModel` only, one file) rather than a
change to `AdminModel::execute_action`'s default, which would touch every
model — the same scoping choice the `TokenAdminModel`/`FeatureFlagAdminModel`
fixes made, left for whoever adds the next model that needs it to decide
with the same shape in hand.

## 📊 Measurement

Tool: `pg_stat_statements` (`calls`, `shared_blks_hit + shared_blks_read`),
reset before the run. Full statement dumps in `baseline/output.txt`
(captured against the pre-fix per-id loop, its own commit) and
`after/output.txt` (captured against the fix, same fixture, same session).

| | before | after |
|---|---:|---:|
| delete CTE statement calls | 615 | **1** |
| delete CTE statement buffers | 17,287 | **14,247** |
| ids submitted (for reference) | 615 | 615 |

Statement count drops from **one per id to one per bulk action** — the
admissible-on-its-own N+1 floor ("statement count per request drops from
O(n) to O(1)... needs no other justification"). Buffers touched also drop
**17.6%** (17,287 → 14,247): every one of the 615 per-id statements paid
its own planning/CTE-setup overhead, its own bitmap-index-scan setup on
both cascading child tables, and, for the 45 pre-deleted + 15 nonexistent
ids, a wasted point-lookup-that-finds-nothing; batching removes 614 of
those redundant setups outright. The N+1 elimination alone clears the
impact floor; the buffer reduction, while real, falls short of the
explicit ≥20% floor on its own — reported here for completeness, not as an
independent floor-clearing claim.

No `temp_blks_written` at any point (no spill, either side, confirmed in
both `output.txt` dumps). No index was added or dropped, so there is no
write-amplification/WAL-tax tradeoff to measure — this changes *how many
statements* carry the same writes, not what they write or what indexes
maintain them. The `autumn_experiment_change_notify` trigger (fires once
per audit row inserted via `pg_notify`) is unaffected: `FOR EACH ROW` fires
the same number of times whether the rows arrive through 615 single-row
deletes or one `id = ANY($1)` delete.

## ✅ Equivalence

The harness computes, for the exact submitted id set, both a final-state
check and an audit-trail check, and prints a deterministic, sorted dump of
every experiment's `name` that received a `'deleted'` audit row this run:

- **Final state**: `COUNT(*) FROM autumn_experiments WHERE id IN (<submitted ids>)`
  is `0` after the action, both before and after the fix — every existing
  id is gone, whether it took 615 statements or 1.
- **Cascade correctness**: `autumn_experiment_assignments`/
  `autumn_experiment_overrides` carry zero rows whose `experiment` no
  longer has a matching `autumn_experiments.name`, both before and after —
  the cascading child-table deletes fire identically whether the parent
  delete arrives as 615 single-id statements or one `id = ANY($1)`
  statement (the harness's own pre-existing-cleanup simulation is run
  through the same cascading CTE, so this check is never vacuously
  satisfied by leftover orphans the fixture itself created).
- **Audit trail**: the number of new `autumn_experiment_changes` rows with
  `mutation = 'deleted'` written after a pre-action watermark equals 555
  (the measured pre-existing count) — **not** 615 — both before and after
  the fix, proving the already-missing 60 ids (45 pre-deleted + 15
  nonexistent) contribute no audit row either way.
- **Deleted-name dump**: the sorted, comma-joined list of the 555 names
  that received a `'deleted'` audit row is **byte-for-byte identical**
  between `baseline/output.txt` and `after/output.txt` (diffed directly
  from the committed artifacts).
- The returned `count` from `execute_action`, asserted equal to `ids.len()`
  (615) in both directions — matching the pre-fix loop's own behavior of
  counting every submitted id as "applied" regardless of whether it
  existed.

No existing test's expectations were edited. The existing
`autumn-admin-plugin` unit-test suite (`experiments::tests::*`) and the
full non-Docker `cargo test -p autumn-admin-plugin` suite pass unchanged
against the fix.

## 💸 Write cost

No index added, dropped, or altered. No WAL/throughput measurement applies
— see Measurement above; this is a round-trip-count (and incidental
per-statement-overhead) change on an existing `DELETE` CTE, not a new write
pattern or a new index to maintain.

## 🔬 Reproduce

```bash
# Full harness (both directions require checking out the respective commit):
cargo test -p autumn-admin-plugin --test experiment_admin_bulk_delete_batch_profile \
  -- --ignored --nocapture --test-threads=1

# Full existing admin-plugin suite (non-Docker):
cargo test -p autumn-admin-plugin
```
