# 🗃️ Ledger: batch the dependent(destroy) leaf cascade (statements 10002→2)

## 🎯 Workload

Any `#[repository(..., dependent(ChildRepo, fk = "...", on_delete = destroy))]`
declaration wires its cascade into the generated `delete_by_id`/`delete_many`
methods — the same methods every `DELETE /api/{resource}/{id}` handler and
bulk-delete action call. `autumn-macros/src/repository.rs`'s
`__autumn_apply_dependent_on_conn` (the shared helper every repository gets)
implements the `Destroy` action as:

```rust
let __ids = /* SELECT id FROM child WHERE fk = $1 ... FOR UPDATE */;
for id in &__ids { /* restrict pre-scan, no-op for a leaf */ }
for id in __ids {
    let __record = child.find(id).first(conn).await?;   // reload
    if let Some(__record) = __record {
        /* hooks, grandchild cascade, counter cache, mutation */
        diesel::delete(child.find(id)).execute(conn).await?;  // delete
    }
}
```

Reachable from every generated single-record and bulk delete path. Read
the existing `dependent`/`has_many` test suite
(`autumn/tests/integration/repository_dependent_destroy.rs`, 3,519 lines,
29 tests) and the CI Docker sweep description in `CLAUDE.md`: this cascade
is exercised heavily and is one of the most structurally central pieces of
generated repository code in the framework — a blog post's comments, a
team's projects, a user's owned rows, anything declared `on_delete =
destroy`.

**Fixture** (`repository_dependent_destroy_leaf_batch_profile.rs`): 500
posts (`ledger_dd_posts`) with a skewed comment fan-out
(`ledger_dd_comments`, ~23,378 rows total, not uniform):
- 485 "ordinary" posts: 5-29 comments each.
- 12 "trending" posts: 200-499 comments each.
- 3 "viral" posts: 2,000 / 3,500 / 5,000 comments.

`author` is 20% NULL, `edited_at` is 70% NULL (real NULL density on both
columns an equivalence check below has to handle), and a follow-up
`UPDATE` before `ANALYZE` gives the table real dead tuples. The measured
operation is a single `delete_by_id` on the 5,000-comment post — the same
statement shape a 5-comment post's delete pays, just with the tax made
visible by a realistic fan-out.

`LedgerDdComment` (the child) is a plain leaf: no hooks, not soft-delete,
not versioned, no counter cache, no `dependent(...)` of its own — the
common case, not a contrived one.

**Reproduce**:
```bash
cargo test -p autumn-web --features "test-support" \
  --test integration_tests -- --ignored \
  repository_dependent_destroy_leaf_batch_profile \
  --nocapture --test-threads=1
```
Requires Docker (`postgres:16-alpine` testcontainer, `pg_stat_statements`
preloaded). Lives in `autumn/tests/integration/`, gated
`#[cfg(feature = "db")]` and `#[ignore = "requires Docker
(testcontainers)"]` — CI's "Run Docker-dependent tests" step sweeps it
automatically (CLAUDE.md), no workflow edit needed.

## 📈 Profile

Single workload, single statement family (every statement touching
`ledger_dd_comments` during one `delete_by_id` call) — there is no
cross-statement ranking to build, the cascade **is** the measured cost.
This is exactly the `pg_stat_statements.calls` signal the Ledger process
calls out as invisible in a buffer ranking but dominant in practice:
individually cheap statements (a point SELECT, a point DELETE), collectively
one per row destroyed.

## 🧭 Plan

**Before** (`baseline/output.txt`):
```
calls=5000   buffers=20454    SELECT ... FROM "ledger_dd_comments" WHERE ("ledger_dd_comments"."id" = $1) LIMIT $2 FOR UPDATE
calls=5000   buffers=20000    DELETE FROM "ledger_dd_comments" WHERE ("ledger_dd_comments"."id" = $1)
calls=1      buffers=63       SELECT $2 FROM ONLY "public"."ledger_dd_comments" x WHERE $1 = "post_id" FOR KEY SHARE OF x
calls=1      buffers=5066     SELECT id FROM "ledger_dd_comments" WHERE "post_id" = $1 ORDER BY id FOR UPDATE
-- total: calls=10002 buffers=45583 --
```

**After** (`after/output.txt`):
```
calls=1      buffers=63       SELECT $2 FROM ONLY "public"."ledger_dd_comments" x WHERE $1 = "post_id" FOR KEY SHARE OF x
calls=1      buffers=5063     DELETE FROM "ledger_dd_comments" WHERE "post_id" = $1
-- total: calls=2 buffers=5126 --
```

The `FOR KEY SHARE OF x` statement in both dumps is Postgres's own internal
FK-integrity check on the parent `DELETE FROM ledger_dd_posts` (verifying no
referencing row remains) — inherent to having a foreign key at all, present
identically before and after, and outside this fix's control.

Each `output.txt` also carries two diagnostic `EXPLAIN (ANALYZE, BUFFERS,
VERBOSE, SETTINGS)` runs against post 499 (3,500 comments), rolled back:
the post-fix batched shape (`DELETE ... WHERE post_id = $1`, a `Bitmap Heap
Scan` off `ledger_dd_comments_post_id_idx`, `Buffers: shared hit=7045`,
2.9ms) and the pre-fix loop's per-row shape (a point `Index Scan` on the
primary key, `Buffers: shared hit=145` — cheap *per call*, paid 3,500
times over for that post alone, this diagnostic runs it once for scale
reference).

## 💡 Hypothesis

"The `Destroy` cascade's Phase 2 reloads every child row
(`child.find(id).first(conn)`) before deleting it
(`diesel::delete(child.find(id))`) — two statements per id, regardless of
whether anything in that configuration reads the reloaded record. For a
leaf child (no hooks, not soft-delete, not versioned, no commit-hook/
broadcast bookkeeping, no `dependent(...)` of its own), nothing does: the
plain-hard-delete arm of the mutation (`#destroy_mutation`) deletes by id
alone. The mechanism can be reduced because the codebase already has a
proven, batched replacement for exactly this configuration —
`autumn_web::repository::dependent_delete_all`, the runtime helper
`on_delete = delete_all` already calls, which does a single
`DELETE FROM child WHERE fk = $1` and (only if the child has counter
caches) a single batched decrement via `counter_cache_before_delete_many`
instead of one decrement per row. `Destroy` never routed to it because
`Destroy` also has to support hooks, soft-delete, versioning, and
grandchild recursion that `DeleteAll` deliberately doesn't — but when none
of those apply, `Destroy` and `DeleteAll` produce identical observable
database effects, so the leaf case can use the same runtime helper."

## 🔧 Change

`autumn-macros/src/repository.rs`: `__autumn_apply_dependent_on_conn`'s
`Destroy` arm now branches at **macro-expansion time** (all four
conditions are known from the child's own `#[repository(...)]` attributes,
not the caller's):

```rust
let destroy_fast_path_eligible = !has_dependents          // no repo-attribute grandchildren
    && config.hooks_type.is_none()                        // no before_delete/after_delete
    && !dep_needs_post                                     // not versioned/commit_hooks/broadcasts
    && !config.soft_delete;                                // never soft-deleted
```

When eligible, the generated code adds one more, **runtime**, check —
`#model_name::dependents().is_empty()` — because `has_dependents` only
sees the repository-attribute route (`dependent(...)` on the
`#[repository]` invocation); a model can independently declare
grandchildren via `#[has_many(dependent = ...)]` on its `#[model(...)]`
struct, resolved through an inherent `dependents()` method invisible to
this macro invocation. That check is a static-slice lookup, no DB round
trip. Only when *both* hold does it call `dependent_delete_all`; otherwise
(including every pre-existing configuration — hooks, soft-delete,
versioned, broadcasting, repo-attribute or model-attribute grandchildren)
it falls through to the exact, unmodified per-row loop:

```rust
if destroy_fast_path_eligible {
    if #model_name::dependents().is_empty() {
        dependent_delete_all(conn, __table, cc_specs, cc_has, __fk_column, __parent_id).await?;
        Ok(destroy_ret_value)
    } else {
        /* unchanged per-row loop — runtime grandchildren still need it */
    }
} else {
    /* unchanged per-row loop */
}
```

No new SQL was written for this fix: `dependent_delete_all` already
existed, already shipped behind `on_delete = delete_all`, and already had
its own test coverage and its own batched counter-cache path
(`counter_cache_before_delete_many`, itself already used and tested by the
`delete_many` bulk-delete path). This just gives `Destroy` a second,
narrower way to reach the same helper `DeleteAll` already reaches, exactly
when `Destroy`'s extra guarantees (hooks, soft-delete, versioning,
recursion) aren't in play.

No migration: no schema or index change, no new statement shape besides
one already in production use behind `delete_all`.

## 📊 Measurement

Tool: `pg_stat_statements` (`calls`, `shared_blks_hit + shared_blks_read`),
reset before the measured `delete_by_id` call. Full dumps in
`baseline/output.txt` (its own commit, pre-fix) and `after/output.txt`
(post-fix, same fixture, same session).

| | before | after |
|---|---:|---:|
| `ledger_dd_comments` statement calls | 10,002 | **2** |
| `ledger_dd_comments` statement buffers | 45,583 | **5,126** |
| comments cascaded | 5,000 | 5,000 |

Statement count: **10,002 → 2**, i.e. O(N) → O(1) — an N+1 elimination,
admissible on its own per the Ledger process ("statement count per unit of
work drops from O(n) to O(1)... needs no other justification"). Buffers:
**45,583 → 5,126, a 88.8% reduction** — comfortably clears the ≥20% floor
independently. Both floors are cleared by the same change; neither is
manufactured — 2 of the 5,126 post-fix buffers are the FK-check statement
shared with the baseline, so essentially all of the reduction is the
cascade itself. No `temp_blks_written` at any point (no spill, either
side, confirmed in both dumps).

## ✅ Equivalence

The harness asserts, for the exact fixture and the exact deleted post,
both before and after the fix (same assertions, same file, run against
both commits):

- **Cascade correctness**: `COUNT(*) FROM ledger_dd_comments WHERE post_id
  = <deleted post>` is `0` after the delete.
- **Parent correctness**: the post row itself is gone
  (`COUNT(*) FROM ledger_dd_posts WHERE id = <deleted post>` is `0`).
- **No collateral damage**: `COUNT(*) FROM ledger_dd_comments` after the
  delete equals the pre-delete total minus exactly the 5,000 cascaded
  rows — no more, no less.
- **Sibling untouched**: a different post's comment count (post 1,
  chosen because its comments include NULL `author` and NULL `edited_at`
  rows — the NULL-density edge case) is identical before and after the
  delete, both with and without the fix.
- `delete_by_id` returns `Ok(())` in both directions — the cascade's
  success/failure surface is unchanged.

No existing test's expectations were edited. The full existing
`dependent`/`has_many` correctness suite
(`repository_dependent_destroy.rs`, including the `dia_*`/`dhr_*` diamond
graphs that reach a *shared* leaf table through two different cascade
edges, and the hook-firing/soft-delete/versioned/cycle-guard cases) passes
unchanged against the fix — 29/29 tests, `cargo test -p autumn-web
--features "db,test-support" --test integration_tests -- --ignored
dependent_destroy --nocapture --test-threads=1`. Every one of those
scenarios is compile-time excluded from the new fast path by construction
(each declares hooks, soft-delete, or grandchildren), so they continue to
run the untouched per-row loop; their being green is a regression check on
the surrounding, unmodified code, not evidence for the fast path itself —
that is what this report's own harness's equivalence checks establish.

The diamond case deserves a direct note since it looked like the likeliest
place for a hidden bug: `dia_leaves`/`dhr_leaves` (the shared leaf reached
from both a soft and a hard branch of the same root) are both
`soft_delete` models, so they are excluded from the fast path by the
`!config.soft_delete` guard — the two trickiest existing tests in the
suite are exactly the ones this change cannot touch, by construction, not
by accident.

## 💸 Write cost

No index added, dropped, or altered. No WAL/throughput measurement
applies: this changes how many statements carry an existing `DELETE`, not
what it writes or what indexes maintain it. The child model in the
qualifying configuration is, by definition, never soft-deleted (the guard
requires `!config.soft_delete`), so there is no soft-vs-hard write-path
distinction introduced either.

## 🔬 Reproduce

```bash
# Full harness (both directions require checking out the respective commit):
cargo test -p autumn-web --features "test-support" \
  --test integration_tests -- --ignored \
  repository_dependent_destroy_leaf_batch_profile \
  --nocapture --test-threads=1

# Full dependent/has_many regression suite (Docker, same binary):
cargo test -p autumn-web --features "db,test-support" \
  --test integration_tests -- --ignored \
  dependent_destroy --nocapture --test-threads=1

# Lint/format:
cargo fmt --all -- --check
cargo clippy -p autumn-macros --all-targets -- -D warnings
cargo clippy -p autumn-web --features "db,test-support" --tests -- -D warnings
```
