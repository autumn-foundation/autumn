# 🗃️ Ledger: batch `autumn generate admin`'s default bulk delete (statements N→1, generator-wide)

## 🎯 Workload

`POST /admin/{slug}/actions` (`autumn-admin-plugin/src/routes.rs`,
`model_action`) is the admin panel's bulk-action endpoint: it parses a form
body carrying `action=<name>` plus a **repeated, uncapped** `ids=<id>` field
(one entry per row an operator selected in the list view), then calls
`model.execute_action(&pool, &action, ids)`.

[`AdminModel::execute_action`](../../../autumn-admin-plugin/src/traits.rs)
(the default every model gets unless it overrides the method) dispatches
`"delete"` by looping over `ids` and calling the model's single-row
`delete` once per id:

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

Three of this framework's own bundled admin models already got bespoke
overrides for this in prior Ledger PRs:
[`TokenAdminModel`](../2026-08-31-ledger-admin-bulk-delete-batch/),
[`FeatureFlagAdminModel`](../2026-09-06-ledger-feature-flag-admin-bulk-delete-batch/),
and `ExperimentAdminModel`
(`docs/reports/2026-09-07-ledger-experiment-admin-bulk-delete-batch/`). Every
one of those fixes was a hand-written override on a single, framework-internal
model. But **every app that runs `autumn generate admin`** — the code
generator every user of this framework reaches for to get an admin panel over
their own models — still hits the unfixed trait default: `render_admin_file`
(`autumn-cli/src/generate/admin.rs`) never emitted an `execute_action`
override at all. This is the generator every real user app's admin panel
comes from, so this gap is the one that actually matters at scale — not a
third bespoke model, the template itself.

**Fixture**: a real generated project. `autumn new` → `autumn generate
scaffold Post title:String body:Text published:bool` → `autumn generate admin
Post title:String body:Text published:bool`, wired to `autumn-admin-plugin`
exactly as `docs/guide/admin.md` instructs (and
`scaffold_encrypted.rs::encrypted_admin_scaffold_cargo_checks` already proves
compiles), then driven through a tiny test-only probe module that calls the
*actual compiled* generated `PostAdmin::execute_action(&pool, "delete", ids)`
— the same call `model_action` makes, skipping only HTTP form-decoding, same
as the three precedent harnesses. 50,000-row `posts` table, `published`
skewed 15% true (mirroring the other admin-bulk-delete fixtures' "most rows
are long-tail" shape), and real dead tuples from a follow-up `UPDATE` before
`ANALYZE` (no `VACUUM`). `posts` has no nullable column (the generator emits
`title`/`body`/`published` all `NOT NULL`), so there is no NULL-density axis
to vary. Bulk-delete selection: 2,000 ids, every 25th across the table (not a
contiguous head block) — every one guaranteed to exist exactly once, so the
before/after statement-count comparison is apples-to-apples (see
✅ Equivalence for why a selection with duplicates/misses is *not*
comparable across the fix).

**Reproduce**:
```bash
cargo test -p autumn-cli --test cli_tests -- --ignored \
  admin_generator_bulk_delete_batches_the_default_execute_action \
  --nocapture --test-threads=1
```
Requires Docker (spins up a `postgres:17-alpine` testcontainer with
`pg_stat_statements` preloaded) **and** compiles a whole separate generated
project against this workspace's local `autumn-web`/`autumn-admin-plugin` —
slower than a typical Docker-gated test in this sweep. Per CLAUDE.md, a test
that is unavoidably both Docker-gated and cold-start-compile runs in the
(slower, but not silently dark) Docker sweep rather than getting new CI
wiring for one test — `#[ignore = "requires Docker (testcontainers)"]` is
enough; no workflow edit was made.

## 📈 Profile

This harness drives exactly one workload — the bulk-delete action — so
there's no cross-statement ranking to build: the `DELETE` (whichever shape it
takes) is the entire measured cost. It is not a small slice of a bigger
request; it **is** the request. The relevant signal is `pg_stat_statements`
`calls` against one well-known statement shape — the same "individually
trivial, collectively dominant" signature the Ledger process calls out as
where N+1s hide.

## 🧭 Plan

Same access method either side — a primary-key point/array lookup via
`posts_pkey`, no seq scan, no sort:

**Before** (`baseline/output.txt:540`, `calls=2000`):
```
DELETE FROM "posts" WHERE ("posts"."id" = $1)
```

**After** (`after/output.txt:540`, `calls=1`):
```
DELETE FROM "posts" WHERE ("posts"."id" = ANY($1))
```

The illustrative `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` samples in
`after/output.txt` show the identical index-scan shape either way — a point
`Index Cond: (posts.id = 1)` for a single id vs. `Index Cond: (posts.id = ANY
('{1,2,3,4,6}'::bigint[]))` for a batch — same access method, same plan
shape, one executor invocation covering the whole array instead of N
separate ones.

## 💡 Hypothesis

"`execute_action`'s default `"delete"` branch is a
`for id in ids { self.delete(&pool, id).await?; }` loop (traits.rs) — one DB
round trip *and* one connection checkout per id. Every `AdminModel` that
`autumn generate admin` produces inherits this because `render_admin_file`
(`autumn-cli/src/generate/admin.rs`) never emits an `execute_action`
override. The fix is mechanical and generator-level: emit one alongside
`delete()` in the template, batching the `"delete"` action into a single
`DELETE ... WHERE id = ANY($1)`, so every future `autumn generate admin` run
gets it for free."

## 🔧 Change

`autumn-cli/src/generate/admin.rs`, `render_admin_file`: the generated
`impl AdminModel for {Model}Admin` now also emits an `execute_action`
override. The `"delete"` branch issues one
`DELETE FROM {plural} WHERE id = ANY($1)` bound to the full `ids: Vec<i64>`
and returns the number of rows the `DELETE` **actually matched** (via
`.execute()`'s row count), not `ids.len()`. This is a deliberate difference
from the `TokenAdminModel`/`FeatureFlagAdminModel` precedents (which return
`ids.len()`, matching their own idempotent `delete()`'s never-errors
semantics) — the generated `delete()` errors `AdminError::NotFound` on a
missing id, so the trait-default loop this replaces silently aborts
mid-batch (and leaves everything deleted *before* the miss committed) the
moment a submitted id doesn't exist. The batched form instead treats a
missing or duplicate id as a safe no-op and reports exactly how many rows
were removed — the same "missing selection is a no-op" contract this
codebase's own scaffolded (`autumn generate scaffold`) bulk-delete route and
the two precedent admin-model overrides already establish, and a strictly
better-defined outcome than the order-dependent partial application it
replaces (see ✅ Equivalence).

`{Model}Admin` never declares soft delete (the generator never emits
`supports_soft_delete()`), so `actions()` only ever offers `"delete"`; any
other action name falls through to the shared
`dispatch_restore_purge_or_unhandled` helper (`autumn-admin-plugin/src/traits.rs`)
— previously usable only from *within* the `autumn-admin-plugin` crate
(`TokenAdminModel`/`FeatureFlagAdminModel` call it via `crate::traits::...`).
Generated code lives in a downstream app, so this PR also re-exports
`dispatch_restore_purge_or_unhandled` from the crate root and `prelude`
(`autumn-admin-plugin/src/lib.rs`) — a purely additive export, not a
signature change, letting generated code reach the exact same shared
fallback the framework's own hand-written overrides use instead of
duplicating its match arms.

No migration, no new index, no Diesel schema change — this only changes how
many round trips one generated app's bulk-delete action costs, and only for
apps whose admin panel is regenerated (or hand-patched) after this fix
ships; already-generated `src/admin/*.rs` files are ordinary user code this
change does not retroactively touch.

## 📊 Measurement

Tool: `pg_stat_statements` (`calls`, `shared_blks_hit + shared_blks_read`),
reset before the run. Full statement dumps in `baseline/output.txt`
(captured against the pre-fix `render_admin_file`, same commit tree with
only `autumn-cli/src/generate/admin.rs` reverted) and `after/output.txt`
(captured against the fix, same fixture, same session — same Postgres
17-alpine instance, same 50,000-row seed, same 2,000-id selection).

| | before | after |
|---|---:|---:|
| `DELETE` statement calls | 2,000 | **1** |
| `DELETE` statement buffers | 9,683 | **5,667** |
| ids submitted (for reference) | 2,000 | 2,000 |

Statement count drops from **one per id to one per bulk action** — the
admissible-on-its-own N+1-elimination floor. Buffers also drop **41.5%**
(9,683 → 5,667, well past the explicit ≥20% floor): unlike the
`TokenAdminModel` case (a bare idempotent `UPDATE`, where folding into
`id = ANY($1)` did the same per-row work plus the array match and buffers
came out roughly flat), here every one of the 2,000 per-id `DELETE`
statements paid its own planning/execution setup on top of an identical
per-row index-scan-and-heap-delete cost; batching removes 1,999 of those
redundant setups, and this table's fixture has no secondary index or
trigger fan-out to inflate the per-row cost on either side. This clears the
impact floor twice over: the N+1 elimination alone needs no other
justification, and the buffer reduction clears the explicit ≥20% floor too.

No `temp_blks_written` at any point (no spill, either side — confirmed in
both `output.txt` dumps, and `posts` at 50,000 rows / 2,000-id batch is
nowhere near `work_mem` pressure). No index was added or dropped, so there
is no write-amplification/WAL-tax tradeoff to measure — this changes *how
many statements* carry the same writes, not what they write or what indexes
maintain them.

## ✅ Equivalence

**Primary selection (2,000 ids, all exist, all unique)**: `after/output.txt`
shows exactly the 2,000 targeted ids gone (`remaining == 48,000`,
`still_present-in-selection == 0`, both asserted) and a deterministic sorted
dump of the surviving ids in `1..=500` (`after/output.txt`, "surviving ids"
section) — every non-multiple-of-25 id present, every multiple of 25
absent, exactly as the selection predicts. `baseline/output.txt`'s harness
run panics *after* printing the same primary-selection numbers (at the
`assert_eq!(delete_calls, 1, ...)` line, which is specifically pinning the
*fixed* behavior) — it never reaches the surviving-ids dump, so that
artifact is only in `after/output.txt`; the primary before/after comparison
that matters (statement count, buffers, and — implicitly, since both runs
delete the identical 2,000-id set with no misses or duplicates — final
table state) is exact and apples-to-apples.

**Edge case (ids that don't cleanly exist exactly once)** — characterizes
the fixed contract only, run against a small disposable set of 10 ids (5
existing, 2 extra repeats of one of them, 3 nonexistent) inserted into the
same running fixture after the primary measurement: `after/output.txt`
shows `LEDGER_RESULT=5` (the count of *rows actually deleted*, not
`ids.len()` (10)) and the 5 non-selected sibling rows survive untouched
(`edge_remaining == 5`, asserted). This is a **deliberate, disclosed
behavior difference** from the code it replaces, not a byte-for-byte
equivalence claim: the trait-default loop this fix removes calls
`self.delete(&pool, id).await?` per id with an unguarded `?` — since the
generated `delete()` returns `AdminError::NotFound` when its `DELETE`
matches zero rows, the *first* missing or already-consumed id in a
submitted batch aborts the whole action immediately, silently leaving every
id **before** the miss deleted and every id **after** it untouched (an
order-dependent partial application, not a designed contract — no existing
test in this repository pins it, confirmed by search before this change).
The batched form instead treats a non-matching id as a no-op and reports
the true deleted-row count — the same "a missing selection is a no-op"
contract this repository's own scaffolded (`autumn generate scaffold`)
bulk-delete route already uses (a pre-flight `SELECT` filters the id list
before `delete_many` ever runs) and that `TokenAdminModel`/
`FeatureFlagAdminModel`'s own overrides establish for their idempotent
`delete()`s. This trade only matters when an operator's selection has
gone stale (a row already deleted by someone else, or a double-submitted
id) — a case the fixed behavior now handles safely and deterministically
instead of failing in an order-dependent way.

Existing tests pass **unchanged**: the full non-Docker `cargo test -p
autumn-cli` suite, `cargo test -p autumn-admin-plugin` (including
`token_admin_bulk_delete_batch_profile`, `feature_flag_admin_bulk_delete_batch_profile`,
`experiment_admin_bulk_delete_batch_profile`, and the rest of the Docker
suite), and `autumn-cli`'s existing `generate admin` unit/generator tests
(none of which asserted anything about `execute_action`, confirmed before
writing this fix — see 🔧 Change).

## 💸 Write cost

No index added, dropped, or altered. No WAL/throughput measurement applies
here in the traditional sense — this is a round-trip-count change on an
existing `DELETE`, not a new write pattern or a new index to maintain. The
41.5% buffer drop reported above is itself a *read*-cost reduction (planning
and index-scan setup avoided 1,999 times over), not a write-cost one.

## 🔬 Reproduce

```bash
# Full harness (both directions require checking out the respective commit
# of autumn-cli/src/generate/admin.rs):
cargo test -p autumn-cli --test cli_tests -- --ignored \
  admin_generator_bulk_delete_batches_the_default_execute_action \
  --nocapture --test-threads=1
```

`baseline/output.txt` is this harness's stdout on the pre-fix
`render_admin_file` template; `after/output.txt` is the same command's
stdout after the fix, same fixture, same session — both committed in full.
