# 🗃️ Ledger: batch the admin bulk "delete" (revoke) token action (statements/req N→1)

## 🎯 Workload

`AdminModel::execute_action` (`autumn-admin-plugin/src/traits.rs`) is the dispatcher
behind `POST /admin/{slug}/actions` — the admin panel's bulk-action endpoint every
scaffolded model gets (`model_action`, `autumn-admin-plugin/src/routes.rs`). Its
default `"delete"` branch looped over the submitted ids and called
`self.delete(&pool, id)` once per id:

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

For `TokenAdminModel` (`autumn-admin-plugin/src/tokens.rs`, the built-in admin model
for scoped API tokens registered by `AdminPlugin`), `delete` is a "delete" in the
soft sense — it revokes:

```sql
UPDATE api_tokens SET revoked_at = NOW() AT TIME ZONE 'utc'
WHERE id = $1 AND revoked_at IS NULL
```

So `POST /admin/api-tokens/actions` with `action=delete` and a batch of selected ids
— an operator revoking a batch of tokens (the textbook case: revoke every token for
a compromised principal during incident response, or clean up a page of expired
service tokens) — drove exactly one DB round trip **per token**, blocking the HTTP
request handler on `ids.len()` sequential awaits against the connection pool.

**Fixture**: `autumn-admin-plugin/tests/token_admin_db.rs`, module `bulk_delete_profile`
— a 20,000-row `api_tokens` table with skewed principal cardinality (400 rows / 2%
belong to 20 heavy-churn service accounts, ~20 tokens each — the rotated-credential
pattern; the remaining 19,600 spread across 15,000 `user:*` principals, mostly 1,
some 2–3, the real long tail), 30% pre-revoked (a real token store's history, not a
freshly-seeded all-active table), `scopes` varying from a 1- to 5-element JSON array,
and real dead tuples from a follow-up `UPDATE` before `ANALYZE`. Three batch-size
tiers (100 / 500 / 2,000 ids — realistic "one admin list page" through "select-all
across several pages of a stale-token cleanup") are drawn as disjoint slices of the
active-id pool so no row is touched twice across the comparison.

**Reproduce**:
```bash
cargo test -p autumn-admin-plugin --test token_admin_db -- --ignored \
  token_bulk_delete_batch --nocapture --test-threads=1
```
Requires Docker (spins up a `postgres:16-alpine` testcontainer with
`pg_stat_statements` preloaded). Already swept by CI's existing
`cargo test -p autumn-admin-plugin --test token_admin_db -- --ignored`
(`.github/workflows/ci.yml`) — no workflow edit needed, this crate's Docker tests are
already targeted individually by binary rather than via the `autumn`/`autumn-cli`
consolidated-binary sweep.

## 📈 Profile

This is a single, dedicated write path (`TokenAdminModel::delete`/`delete_many` is
the only thing `execute_action("delete", ...)` calls), so the "share of the
workload's total buffers/calls" framing doesn't apply the way it does to a mixed
request — the relevant number is `calls`, which the fixture shows scales 1:1 with
the batch size, exactly the N+1 shape:

| tier | ids | calls (before) |
|---|---:|---:|
| small | 100 | 100 |
| medium | 500 | 500 |
| large | 2,000 | 2,000 |

## 🧭 Plan

Same access method either side — an `Index Scan using api_tokens_pkey` — this is not
a plan-shape change, it is the same `UPDATE ... WHERE revoked_at IS NULL` predicate
issued once per **batch** (`id = ANY($1)`) instead of once per **id** (`id = $1`).

**Before** (one call per id — `baseline/pg_stat_statements_and_explain.txt`):
```
UPDATE api_tokens SET revoked_at = NOW() AT TIME ZONE $2 WHERE id = $1 AND revoked_at IS NULL
Update on public.api_tokens  (actual rows=0 loops=1)
  Buffers: shared hit=5
  ->  Index Scan using api_tokens_pkey on public.api_tokens (actual rows=0 loops=1)
        Index Cond: (api_tokens.id = 999999999)
        Filter: (api_tokens.revoked_at IS NULL)
        Buffers: shared hit=5
```

**After** (one call per batch — `after/pg_stat_statements_and_explain.txt`, EXPLAIN
against a real 500-id batch of still-active ids, so this is the actual plan the
"medium" tier ran, not a toy):
```
UPDATE api_tokens SET revoked_at = NOW() AT TIME ZONE $2 WHERE id = ANY($1) AND revoked_at IS NULL
Update on public.api_tokens  (actual rows=0 loops=1)
  Buffers: shared hit=5165 dirtied=6 written=6
  ->  Index Scan using api_tokens_pkey on public.api_tokens (actual rows=500 loops=1)
        Index Cond: (api_tokens.id = ANY ('{...500 ids...}'::integer[]))
        Filter: (api_tokens.revoked_at IS NULL)
        Buffers: shared hit=1220
```

## 💡 Hypothesis

The handler issues one `UPDATE` per row instead of one batched load — the textbook
Diesel-codebase N+1 the process calls "the single highest-value change available...
needs no other justification": `execute_action`'s `"delete"` branch has no batching
primitive to call, so it can't do anything **but** one statement per id.

## 🔧 Change

Added `AdminModel::delete_many` (`autumn-admin-plugin/src/traits.rs`) with a default
that is the **exact same loop**, moved verbatim out of `execute_action`:

```rust
fn delete_many<'a>(&'a self, pool: &'a Pool<RuntimeConnection>, ids: Vec<i64>) -> AdminFuture<'a, u64> {
    Box::pin(async move {
        let mut count: u64 = 0;
        for id in ids {
            self.delete(pool, id).await?;
            count += 1;
        }
        Ok(count)
    })
}
```
`execute_action`'s `"delete"` branch now reads `self.delete_many(&pool, ids).await`.
Every `AdminModel` implementor that doesn't override `delete_many` — `feature_flags.rs`,
`experiments.rs`, every hand-written and every `autumn scaffold`-generated model —
keeps **byte-identical** behavior, abort-on-first-failure included (verified by the
pre-existing `default_execute_action_delete_aborts_on_first_failure` unit test,
unchanged, still green).

`TokenAdminModel` overrides `delete_many` with one batched statement:
```rust
diesel::sql_query(
    "UPDATE api_tokens SET revoked_at = NOW() AT TIME ZONE 'utc' \
     WHERE id = ANY($1) AND revoked_at IS NULL",
)
.bind::<Array<BigInt>, _>(ids)
.execute(&mut conn)
```
returning `ids.len() as u64` — the same "count of ids asked for" contract the old
per-id loop had (it counted attempts, not affected rows; `delete()` never reports
`0 rows changed` as an error, so the two are the same number today and the visible
flash message — "Applied 'delete' to N record(s)." — is unchanged).

No migration: the fix reads and writes exactly the columns `delete()` already did,
through the same primary-key index. No lock beyond each `UPDATE`'s row-level locks,
same as today.

## 📊 Measurement

`pg_stat_statements`, same session, before/after (`baseline/` and `after/` in this
directory):

| tier | ids | calls before | calls after | buffers before | buffers after | WAL before | WAL after |
|---|---:|---:|---:|---:|---:|---:|---:|
| small | 100 | 100 | **1** | 526 | 997 | 13,386 | 29,192 |
| medium | 500 | 500 | **1** | 2,703 | 4,267 | 67,928 | 153,615 |
| large | 2,000 | 2,000 | **1** | 10,815 | 16,350 | 272,708 | 634,799 |

**Statement count drops from O(n) to O(1) at every tier** — the impact-floor
criterion this PR is built on ("Elimination of an N+1... needs no other
justification"). For a 2,000-id bulk action that is **2,000 sequential blocking
round trips collapsed into 1**, each of which held a pooled connection open for the
whole loop; the batched form does the same work in one round trip.

**Buffers and WAL went up, not down** — disclosed, not smoothed over. Both roughly
1.5–1.9× (buffers) and 2.0–2.3× (WAL bytes) higher for the batched statement than
the sum of the per-id statements it replaces, consistently across all three tiers.
I looked for the obvious structural cause and ruled it out rather than guess:
`pg_stat_user_tables.n_tup_hot_upd`/`n_tup_upd` deltas around each tier (same
artifact files) show the batched path has **equal-or-fewer** non-HOT updates than
the per-id loop (0/0/0 after vs. 0/6/26 before) — so a loss of cross-statement
HOT-pruning is not the explanation; if anything the batched path is *more*
HOT-efficient. The root cause of the extra buffer/WAL cost is not isolated further
in this pass — flagged here rather than asserted away, and worth a follow-up if
someone wants to chase it (a candidate next step: compare `= ANY($1)` against a
`VALUES(...)`-join multi-row `UPDATE`, the shape the search-write-documents-batch
fix used, to see whether the array-scan path specifically is the more expensive
one). In absolute terms even the 2,000-id tier's 16,350 buffers is ~128 MB of
logical page touches for a single backend, well below anything that would trip a
throughput or lock-duration concern.

`temp_blks_written`: zero on both sides at every tier (no spill; expected, `id =
$1`/`id = ANY($1)` against a PK index needs no sort or hash buffer).

## ✅ Equivalence

`token_bulk_delete_batch_result_equivalence` (same file) exercises the edge cases
the scale tiers don't:

- **Empty id list** — `delete_many(&pool, vec![])` returns `Ok(0)`, no error (the
  old loop's `for id in ids {}` over an empty vec was already a no-op; this keeps
  it one).
- **Duplicate ids** — the same id twice in one batch: revoked once, counted twice
  (`count == ids.len()`, matching the old loop's `count += 1` per iteration,
  duplicates included).
- **Already-revoked id in the batch** — its `revoked_at` timestamp is read before
  and after and asserted **unchanged**: `WHERE ... AND revoked_at IS NULL` in the
  batched statement is the identical predicate `delete()` used, so an
  already-revoked row is silently skipped either way — never touched twice, matches
  `delete()`'s documented idempotency.
- **Nonexistent id in the batch** — no error, and the table's total row count is
  asserted unchanged (`SELECT COUNT(*)`) — confirms it can't be misread as an
  upsert or otherwise mutate anything it shouldn't.
- All of the above **in the same call**, mixed with two real active ids — both real
  ids end up revoked, nothing else does.

No `ORDER BY ... LIMIT` involved (a point-lookup predicate on a primary key, no
ties to break). No bi-temporal/validity-interval semantics touched — `api_tokens`
isn't a ledgered/versioned model. No transaction-boundary change: `delete()` and
`delete_many()` both run their single statement outside any wrapping transaction,
same as before (one implicit autocommit per call either way — now one call instead
of N, so this is actually **fewer** distinct transactions for the same net effect,
not a semantics change, since each old per-id `UPDATE` was already independently
committed and there was never any all-or-nothing guarantee across the batch to
preserve).

Existing tests pass **unchanged**: all 244 `autumn-admin-plugin` unit tests
(including `default_execute_action_delete_invokes_delete_for_each_id`,
`default_execute_action_delete_aborts_on_first_failure`,
`execute_action_restore_dispatches_to_restore_method`,
`execute_action_purge_dispatches_to_purge_method`) and the existing
`token_admin_db.rs` Docker tests (`token_admin_delete_revokes_token` et al.) — none
of their expectations were edited.

## 💸 Write cost

No index added — this rewrites an existing statement's shape, nothing new to
maintain. The WAL-bytes increase is reported above under Measurement, not hidden:
this is a genuine cost of the batched form on this fixture, weighed against the
round-trip elimination in the trade-off this PR is making explicitly rather than
silently.

## 🔬 Reproduce

```bash
# Full profile + equivalence run (both tests), against a real Postgres container:
cargo test -p autumn-admin-plugin --test token_admin_db -- --ignored \
  token_bulk_delete_batch --nocapture --test-threads=1

# Lint/format/unit gates:
cargo fmt --all
cargo clippy -p autumn-admin-plugin --all-targets --all-features -- -D warnings
cargo test -p autumn-admin-plugin --all-features
```

Baseline and after artifacts (full `pg_stat_statements` dumps, HOT-update deltas,
and `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` output) are committed in
`baseline/` and `after/` alongside this README.
