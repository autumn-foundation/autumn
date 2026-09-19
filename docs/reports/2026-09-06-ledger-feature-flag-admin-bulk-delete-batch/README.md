# 🗃️ Ledger: batch FeatureFlagAdminModel bulk delete (statements/action N→1)

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

[`FeatureFlagAdminModel`](../../../autumn-admin-plugin/src/feature_flags.rs)
(the built-in admin model for feature flags, `/admin/feature-flags/`)
didn't override `execute_action`, so it inherited this loop. Its `delete()`
is a `pool.get()` + single-row CTE round trip
(`DELETE ... WHERE id = $1 RETURNING key` feeding an audit `INSERT` into
`feature_flag_changes`) — so an operator selecting hundreds of retired
flags in a quarterly cleanup and clicking "Delete selected" cost one
statement, and one connection checkout, **per flag**, not per click. This
is the same shape `TokenAdminModel` already closed
(`docs/reports/2026-08-31-ledger-admin-bulk-delete-batch/`);
`FeatureFlagAdminModel` was the next model left on the trait default.

**Fixture**: a 4,000-row `autumn_feature_flags` table (the real schema
from `autumn/migrations/20260530200000_create_feature_flags/up.sql`,
included verbatim via `include_str!` so the fixture can't drift from what
the admin UI actually manages) — a plausible size for a long-lived,
many-team app that never prunes retired experiment flags. Keys spread
across 12 team namespaces, 40% NULL `description`, 15% still `enabled`
(most flags accumulate as stale/retired long-tail), rollout percentages
cycling through the same six values the admin UI's `Select` field offers,
and real dead tuples from a follow-up `UPDATE` before `ANALYZE`.
`feature_flag_changes` (the audit log the real `delete()` CTE writes to)
is pre-seeded with 3 rows per flag (12,000 rows) so the audit table is a
realistic size, not empty, when the bulk action adds to it.

The bulk-delete selection is 800 ids — a plausible one-shift "prune every
flag this quarter's cleanup marked dead" operator action — scattered every
5th id across the table (not a contiguous head block), of which 60 are
force-deleted *before* the action runs (a narrower cleanup already caught
them — must stay a no-op, not error), plus 20 ids past the table's range
that never existed at all (same requirement). The exact pre-existing count
(740) is measured with a `COUNT(*)`, not assumed, per the lesson called out
in the `TokenAdminModel` harness this one mirrors.

**Reproduce**:
```bash
cargo test -p autumn-admin-plugin --test feature_flag_admin_bulk_delete_batch_profile \
  -- --ignored --nocapture --test-threads=1
```
Requires Docker (spins up a `postgres:16-alpine` testcontainer with
`pg_stat_statements` preloaded). This crate has no consolidated
`tests/integration/mod.rs` (unlike `autumn`/`autumn-cli`), and CI does not
run a bare `--ignored` sweep over this package either — this binary is
invoked by an explicit `--test feature_flag_admin_bulk_delete_batch_profile`
line in `.github/workflows/ci.yml`, next to the existing `token_admin_*`
lines (both in the "Run Docker-dependent tests" step and the coverage
step) — a bare sweep would silently never compile or run it.

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
`autumn_feature_flags_pkey`, no seq scan, no sort:

**Before** (`baseline/output.txt`, `calls=820`):
```
WITH deleted AS ( DELETE FROM autumn_feature_flags WHERE id = $1 RETURNING key ), _audit AS ( INSERT INTO feature_flag_changes (key, mutation, actor) SELECT key, $2, $3 FROM deleted ) SELECT COUNT(*) AS count FROM deleted
```

**After** (`after/output.txt`, `calls=1`):
```
WITH deleted AS ( DELETE FROM autumn_feature_flags WHERE id = ANY($1) RETURNING key ), _audit AS ( INSERT INTO feature_flag_changes (key, mutation, actor) SELECT key, $2, $3 FROM deleted ) SELECT COUNT(*) AS count FROM deleted
```

Every id-scoped delete CTE, the audit `INSERT` it triggers, and the loop's
own `pool.get()` call collapse into one round trip carrying one bound
`bigint[]` array instead of 820 separately-prepared, separately-executed
statements. The diagnostic `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)`
in both dumps shows the identical per-row plan shape (index scan on
`autumn_feature_flags_pkey`, CTE-scan-driven audit insert, the
`autumn_flag_change_notify` trigger firing once per deleted row) — this is
a round-trip-count change, not a plan-shape change.

## 💡 Hypothesis

"`execute_action`'s default `"delete"` branch is a
`for id in ids { self.delete(&pool, id).await?; }` loop (traits.rs) — one
DB round trip *and* one connection checkout per id.
`FeatureFlagAdminModel::delete` (feature_flags.rs) is a single-statement
CTE: `DELETE ... WHERE id = $1 RETURNING key` feeding
`INSERT INTO feature_flag_changes SELECT key, 'deleted', NULL FROM deleted`.
The fix is mechanical: override `execute_action` on `FeatureFlagAdminModel`
for the `"delete"` action to widen the predicate to `id = ANY($1)` over the
whole id list — the audit `INSERT`'s `SELECT ... FROM deleted` already fans
out to one row per id the `DELETE` actually removed, so no other clause
needs to change."

## 🔧 Change

`autumn-admin-plugin/src/feature_flags.rs`: `FeatureFlagAdminModel` now
overrides `execute_action`. The `"delete"` branch issues one
`WITH deleted AS (DELETE FROM autumn_feature_flags WHERE id = ANY($1) RETURNING key), _audit AS (INSERT INTO feature_flag_changes (key, mutation, actor) SELECT key, 'deleted', NULL FROM deleted) SELECT COUNT(*) AS count FROM deleted`
bound to the full `ids: Vec<i64>`, and returns `ids.len()` — matching the
loop's own counting behavior exactly (see Equivalence). `FeatureFlagAdminModel`
never declares soft delete (`supports_soft_delete()` is the trait default,
`false`), so `actions()` only ever offers `"delete"`; the `"restore"`/
`"purge"`/unhandled-action branches are kept as unchanged copies of the
trait default's per-id loop, purely so a direct or out-of-band call to
`execute_action("restore", …)`/`execute_action("purge", …)` still gets the
exact same "does not support soft delete" error it always did. There is no
batching concern there: `self.restore`/`self.purge` are the trait's default
methods, which return `Err` on the very first id regardless of loop shape.

No migration — the tables and `autumn_feature_flags_pkey` are unchanged;
this only changes how many round trips one bulk action costs. This is a
single, scoped override (`FeatureFlagAdminModel` only, one file) rather
than a change to `AdminModel::execute_action`'s default, which would touch
every model — the same scoping choice the `TokenAdminModel` fix made, left
for whoever adds the next model that needs it to decide with the same
shape in hand.

## 📊 Measurement

Tool: `pg_stat_statements` (`calls`, `shared_blks_hit + shared_blks_read`),
reset before the run. Full statement dumps in `baseline/output.txt`
(captured against the pre-fix per-id loop, its own commit) and
`after/output.txt` (captured against the fix, same fixture, same session).

| | before | after |
|---|---:|---:|
| delete CTE statement calls | 820 | **1** |
| delete CTE statement buffers | 9,639 | **6,977** |
| ids submitted (for reference) | 820 | 820 |

Statement count drops from **one per id to one per bulk action** — the
admissible-on-its-own N+1 floor ("statement count per request drops from
O(n) to O(1)... needs no other justification"). Buffers touched also drop
**27.6%** (9,639 → 6,977): unlike the `TokenAdminModel` case (a bare
`UPDATE`, where folding into `id = ANY($1)` did the same per-row work plus
the array match), here every one of the 820 per-id statements paid its own
planning/CTE-setup overhead and, for the 60 pre-deleted + 20 nonexistent
ids, a wasted point-lookup-that-finds-nothing; batching removes 819 of
those redundant setups outright. This clears the impact floor twice over —
the N+1 elimination alone is admissible, and the buffer reduction clears
the explicit ≥20% floor too.

No `temp_blks_written` at any point (no spill, either side, confirmed in
both `output.txt` dumps). No index was added or dropped, so there is no
write-amplification/WAL-tax tradeoff to measure — this changes *how many
statements* carry the same writes, not what they write or what indexes
maintain them. The `autumn_flag_change_notify` trigger (fires once per
deleted row via `pg_notify`) is unaffected: `FOR EACH ROW` fires the same
number of times whether the rows arrive through 820 single-row `DELETE`s
or one `id = ANY($1)` `DELETE`.

## ✅ Equivalence

The harness computes, for the exact submitted id set, both a final-state
check and an audit-trail check, and prints a deterministic, sorted dump of
every flag's `key` that received a `'deleted'` audit row this run:

- **Final state**: `COUNT(*) FROM autumn_feature_flags WHERE id IN (<submitted ids>)`
  is `0` after the action, both before and after the fix — every
  existing id is gone, whether it took 820 statements or 1.
- **Audit trail**: the number of new `feature_flag_changes` rows with
  `mutation = 'deleted'` written after a pre-action watermark equals 740
  (the measured pre-existing count) — **not** 820 — both before and after
  the fix, proving the already-missing 80 ids (60 pre-deleted + 20
  nonexistent) contribute no audit row either way.
- **Deleted-key dump**: the sorted, comma-joined list of the 740 keys that
  received a `'deleted'` audit row is **byte-for-byte identical** between
  `baseline/output.txt` and `after/output.txt` (diffed directly from the
  committed artifacts).
- The returned `count` from `execute_action`, asserted equal to `ids.len()`
  (820) in both directions — matching the pre-fix loop's own behavior of
  counting every submitted id as "applied" regardless of whether it existed.

No existing test's expectations were edited. The existing
`autumn-admin-plugin` Docker-backed suite (`token_admin_db.rs`,
`impersonation_admin.rs`, `token_admin_bulk_delete_batch_profile.rs`) and
the full non-Docker `cargo test -p autumn-admin-plugin` suite pass
unchanged against the fix.

## 💸 Write cost

No index added, dropped, or altered. No WAL/throughput measurement applies
— see Measurement above; this is a round-trip-count (and incidental
per-statement-overhead) change on an existing `DELETE` CTE, not a new write
pattern or a new index to maintain.

## 🔬 Reproduce

```bash
# Full harness (both directions require checking out the respective commit):
cargo test -p autumn-admin-plugin --test feature_flag_admin_bulk_delete_batch_profile \
  -- --ignored --nocapture --test-threads=1

# Full existing admin-plugin Docker suite:
cargo test -p autumn-admin-plugin -- --ignored
```
