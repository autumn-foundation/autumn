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
an audit `INSERT` into `autumn_experiment_changes`) — one statement, and
one connection checkout, per experiment, not per bulk-action request.

**Reachability, precisely stated** (caught by review): `model_action`
itself places no cap on how many `ids=` entries one POST carries — that's
a route-level property, not a UI one. The *stock* admin list template,
though, only ever emits a checkbox for the current page's rows
(`templates.rs`), and `ExperimentAdminModel` doesn't override
`per_page()`'s 25-row default, so a single click in the shipped UI tops
out at 25 ids, not 615. The 615-id workload this harness measures is real
production traffic reachable through: a scripted/API client hitting the
same unauthenticated-by-shape `POST /admin/experiments/actions` endpoint
directly (the endpoint has no ids cap to bypass); an app that raises
`per_page()` or adds a "select all matching filter" control on top of the
same endpoint (a one-line, common admin-panel feature this codebase
doesn't happen to ship yet); or simply the SAME per-id loop running once
per 25-id page as an operator pages through a large cleanup — 25 requests
of 25 ids apiece pay the identical N+1 tax as one request of 615, just
spread across more round trips of the browser's own. The defect measured
and the fix applied are the same either way: `execute_action`'s per-id
loop runs on every `POST /admin/experiments/actions`, page-sized or not.
This report benchmarks the endpoint's own worst case (one large request)
because that's the shape `TokenAdminModel`'s and `FeatureFlagAdminModel`'s
prior reports already benchmark it at, not because the shipped UI can
submit it in one click today.

This is the same shape already closed for `TokenAdminModel`
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
how wide its rollout was, ~270k rows total, via a fanout modulus coprime to
the bulk-delete selection's own stride so the selected experiments see the
same skew as the fixture as a whole) and 0–3 staff overrides
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

**Before** (`baseline/output.txt`, `calls=615`):
```
WITH deleted AS ( DELETE FROM autumn_experiments WHERE id = $1 RETURNING name ), _del_assignments AS ( DELETE FROM autumn_experiment_assignments WHERE experiment IN (SELECT name FROM deleted) ), _del_overrides AS ( DELETE FROM autumn_experiment_overrides WHERE experiment IN (SELECT name FROM deleted) ), _audit AS ( INSERT INTO autumn_experiment_changes (experiment, mutation, actor) SELECT name, $2, $3 FROM deleted ) SELECT COUNT(*) AS count FROM deleted
```

**After** (`after/output.txt`, `calls=1`):
```
WITH deleted AS ( DELETE FROM autumn_experiments WHERE id = ANY($1) RETURNING name ), _del_assignments AS ( DELETE FROM autumn_experiment_assignments WHERE experiment IN (SELECT name FROM deleted) ), _del_overrides AS ( DELETE FROM autumn_experiment_overrides WHERE experiment IN (SELECT name FROM deleted) ), _audit AS ( INSERT INTO autumn_experiment_changes (experiment, mutation, actor) SELECT name, $2, $3 FROM deleted ) SELECT COUNT(*) AS count FROM deleted
```

Each dump carries two diagnostic `EXPLAIN (ANALYZE, BUFFERS, VERBOSE,
SETTINGS)` runs, both rolled back: the pre-fix single-id shape (`id = 2`),
and — run against the exact `ids` array the real action below submits,
all 615 elements, not a small stand-in array — the real post-fix batched
shape (`id = ANY(ARRAY[...615 ids...])`).

**This is a plan-shape change on the two child-table cascade deletes, not
just a round-trip-count change** — correcting an earlier draft of this
report, which claimed "same plan either side, no seq scan" from an
under-sized 5-element `EXPLAIN` array (caught by review). The parent
delete on `autumn_experiments` itself keeps the same shape either way
(`Index Scan using autumn_experiments_pkey`, single id or `= ANY(...)`).
But the cascading deletes on `autumn_experiment_assignments`/
`autumn_experiment_overrides` genuinely change plan with the real
615-element array:

- **Single id** (baseline diagnostic): `Bitmap Index Scan` on
  `idx_autumn_exp_assignments_experiment`/`idx_autumn_exp_overrides_experiment`
  — one experiment name, so an index probe per cascade.
- **Real 615-id array** (after diagnostic): `Seq Scan` on both child
  tables feeding a `Hash Join` against the 555 distinct deleted names —
  the planner estimates matching 555 names is cheap enough, relative to a
  269,720-row `autumn_experiment_assignments`, that one sequential pass
  building a hash probe beats 555 separate index probes.

That Seq Scan is real, measured work (`Buffers: shared hit=2633` on the
scan itself, `shared hit=52,553` on the enclosing `Delete` node once the
matched 49,920 assignment rows are actually located and removed) — it is
not free, and it is *not* what the impact-floor claim in this report rests
on. The floor is cleared by the N+1 statement-count elimination alone
(615 → 1 calls), which needs no plan-shape argument; this plan-shape
change is reported here because it happened, not because it is the win.
Per the Ledger process, a plan-shape change is only admissible as its own
floor-clearing claim when demonstrated at ≥3 data sizes — this report
demonstrates it at one size, so it is descriptive here, not a second
independent claim.

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
| delete CTE statement buffers | 62,012 | **58,616** |
| ids submitted (for reference) | 615 | 615 |

Statement count drops from **one per id to one per bulk action** — the
admissible-on-its-own N+1 floor ("statement count per request drops from
O(n) to O(1)... needs no other justification"). Buffers touched also drop
**5.5%** (62,012 → 58,616), but not because the cascade work is identical
either way — see 🧭 Plan above: the batched array flips the cascading
assignment/override deletes from 555 separate `Bitmap Index Scan` probes
to one `Seq Scan` + `Hash Join` per child table. That trades 615
statements' worth of repeated planning/CTE-setup overhead and the 60
wasted (pre-deleted + nonexistent) point-lookups-that-find-nothing for two
sequential passes over the child tables — cheaper here (`autumn_experiment_assignments`
is a 269,720-row table, small enough that one seq scan undercuts hundreds
of index probes), but the two are genuinely different plans, not the same
work counted once instead of 615 times. The N+1 elimination alone clears
the impact floor; the buffer reduction, while real, falls well short of
the explicit ≥20% floor on its own — reported here for completeness, not
as an independent floor-clearing claim.

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

## ⚠️ Known limitation (found by review, not fixed here)

Neither `autumn_experiment_assignments` nor `autumn_experiment_overrides`
has a foreign key to `autumn_experiments` (confirmed against the
migration, `autumn/migrations/20260530300000_create_experiments/up.sql`)
— the cascade is hand-written SQL in `delete()`/`execute_action`, not an
`ON DELETE CASCADE` constraint. `PgExperimentStore::record_assignment`
(`autumn/src/experiments.rs`) takes a `pg_advisory_xact_lock` keyed on the
**actor**, not the experiment, before inserting a new assignment row; it
does not coordinate with a concurrent delete on that experiment at all.
So if an operator bulk-deletes a `running` experiment while live traffic
is still calling `assign()`, a `record_assignment`/`set_override` insert
that commits after this statement's `_del_assignments`/`_del_overrides`
CTE has already scanned past that experiment's rows creates an orphaned
assignment/override row the delete never saw — and if that experiment
name is later reused, the new experiment silently inherits it.

**This race pre-dates this PR** and is identical in kind for the
single-id `delete()` this fix replaces — no FK, no shared lock, either
way. What this PR changes is the exposure *window*: the old per-id loop
ran 615 near-instantaneous single-row statements, each closing its own
race window in well under a millisecond; the batched statement runs one
`Seq Scan`-driven cascade that spends ~54ms deleting assignment rows
(see 🧭 Plan), so a concurrent insert has a measurably longer wall-clock
window to land an orphan for *any* of the 555 target experiments, not
just the one instantaneous single-row window each previously had. Under
sustained concurrent write load against a `running` experiment being
deleted, that is a real increase in the probability of hitting the race,
not merely a theoretical one.

Closing it needs one of: an `ON DELETE CASCADE` foreign key (a schema/
migration change — this repo's own contributing rules require a
human-reviewed migration for any lock stronger than `SHARE UPDATE
EXCLUSIVE`, and adding a validated FK to populated tables this size takes
one), or a lock shared between the delete path and
`record_assignment`/`set_override` keyed on the experiment name (a
concurrency/transaction-boundary change). Both are outside "smallest
change that moves the counter" and outside what an admin-model
`execute_action` override can decide on its own — this report surfaces
the finding rather than picking a fix, the same way the Ledger process
already routes the structurally identical `WebhookOutboundManager`
fan-out and `repository_commit_hooks` claim/ack findings to a human
decision.

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
