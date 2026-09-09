# Maintained Derived Read Models: `#[derivation]`

A [counter cache](counter-cache.md) maintains one number: how many live
children a parent has. Applications need narrower numbers too. A blog wants
`posts.published_comment_count`, not every comment. A feed wants
`posts.visible_score`, the sum of the scores of its visible comments.

`#[derivation]` declares such a column on the child model. The framework
maintains it on the parent, in the same transaction as every row mutation:

```rust,ignore
#[autumn_web::model(table = "comments")]
#[belongs_to(Post, fk = post_id)]
#[derivation(Post, column = "published_comment_count", filter = published)]
#[derivation(Post, column = "visible_score", transform = sum(score),
             filter = published && score > 0)]
pub struct Comment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub published: bool,
    pub score: i64,
}
```

A derivation is a counter cache with two extra pieces. Each child row has a
*contribution*: `1` for a count, the named field for a sum, and `0` for a row
the filter rejects. Each derivation has a *filter*, lowered to SQL. Both live in
the same `CounterCacheSpec` the counter cache uses, so every generated
repository mutation maintains derivations with no new code path. A plain counter
cache is the unfiltered special case, and its SQL is unchanged. The result is a
plain column, so reading it for N parents stays one query.

## The attribute

Write `#[derivation]` **below** `#[model]`. `#[model]` consumes it. The first
argument is the parent model type. Every other key follows in any order.

| Key | Default | Meaning |
|---|---|---|
| *(positional)* | **required** | the parent model type, e.g. `Post` |
| `column` | **required** | the maintained column on the parent. A plain identifier |
| `transform` | `count` | `count`, or `sum(<field>)` over a child field |
| `filter` | none | the predicate deciding which child rows contribute |
| `fk` | the one `#[belongs_to]` leg to that parent, else `{snake(Parent)}_id` | the child column naming the parent. Required when two legs point at one parent |
| `parent_table` | inferred from the parent type (`Post` gives `posts`) | the parent's table, for a parent that overrides its own |
| `tenant` | none | tenant-discriminator column, as `counter_cache_tenant`. Must name a field of the child, not renamed with `#[diesel(column_name)]` |
| `name` | `{parent_table}.{column}` | the registry name, used by the state table and the actuator. Non-empty, at most 128 bytes, no control characters, and not under the framework's reserved `parked::` prefix |

Each key may appear once. A repeated key is a compile error rather than a
silent last-one-wins.

`sum(<field>)` requires a non-nullable integer field (`i8`, `i16`, `i32` or
`i64`). A nullable or floating-point sum is a compile error, because the Rust
and the SQL lowering would disagree on it.

Two derivations that maintain one `(parent table, column)` pair on the same
model are a compile error, as is a derivation colliding with a counter cache
there: both would move the column twice. Across models, that collision and two
derivations sharing a `name` are caught by the registry check every entry point
runs, including the boot path before it opens a connection, so the process stops
with both module paths named rather than double-counting or sharing one state
row.

## The filter grammar

One filter declaration produces two lowerings. The record paths evaluate the
Rust predicate. The set-based paths splice the SQL predicate. `{c}` is the
placeholder for whichever alias the statement gives the child table.

| Filter | Rust predicate | SQL predicate |
|---|---|---|
| `f` (`bool`) | `__r.f` | `{c}."f" = TRUE` |
| `f` (`Option<bool>`) | `__r.f == Some(true)` | `{c}."f" = TRUE` |
| `!f` (`bool`) | `!__r.f` | `{c}."f" = FALSE` |
| `!f` (`Option<bool>`) | `__r.f == Some(false)` | `{c}."f" = FALSE` |
| `f == true` / `f == false` / `f != true` | folds onto the two rows above | same |
| `f > 3` (`i64`, negative literals allowed) | `__r.f > 3` | `{c}."f" > 3` |
| `f > 3` (`Option<i64>`) | `__r.f.is_some_and(\|v\| v > 3)` | `{c}."f" > 3` |
| `f != 3` (`Option<i64>`) | `__r.f.is_some_and(\|v\| v != 3)` | `{c}."f" <> 3` |
| `f == "pub"` (`String`) | `__r.f == "pub"` | `{c}."f" = 'pub'` |
| `f == "pub"` (`Option<String>`) | `__r.f.as_deref() == Some("pub")` | `{c}."f" = 'pub'` |
| `f != "pub"` (`Option<String>`) | `__r.f.as_deref().is_some_and(\|v\| v != "pub")` | `{c}."f" <> 'pub'` |
| `f.is_some()` | `__r.f.is_some()` | `{c}."f" IS NOT NULL` |
| `f.is_none()` | `__r.f.is_none()` | `{c}."f" IS NULL` |
| `a && b`, and parentheses | `(a) && (b)` | `(a) AND (b)` |

A field may be `bool`, an integer or `String`, or the `Option` form of one of
those. Every `Option` form follows SQL's NULL semantics: a NULL row satisfies no
comparison, so it contributes nothing. That is why an `Option` inequality lowers
to `is_some_and` rather than `!=`, which in Rust would count the NULL row SQL
excludes.

A string literal is single-quoted for SQL, and an embedded `'` is doubled. A
literal `{`, `}`, `\`, NUL or other control character in a string is
rejected: a brace could forge the `{c}` placeholder, and the others have
backend-specific escape rules. Ordering comparisons on a string field are
rejected too: Rust compares bytes and SQL compares by collation, so
`status > "b"` would mean two different things in the two lowerings. `==` and
`!=` are lowered as `CAST(col AS TEXT) = 'lit'` under the backend's bytewise
collation (`COLLATE "C"` on Postgres, `COLLATE BINARY` on SQLite), so a column
declared `NOCASE`, with a case-folding collation, or as Postgres `citext`
(whose own equality operator folds case whatever the collation says) still
compares the way Rust does: `"PUB"` is not `"pub"` on either side.

Everything else is a compile error whose message lists the grammar: `||`,
arithmetic, any method call other than the two NULL probes, float literals, a
name that is not a field, and a field of an unsupported type. A field renamed by
`#[diesel(column_name = "...")]` is rejected as well, because the lowered SQL
names the column after the Rust field.

## The required migration

The parent column is yours to create, exactly as for a counter cache:

```sql
ALTER TABLE posts ADD COLUMN published_comment_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE posts ADD COLUMN visible_score BIGINT NOT NULL DEFAULT 0;
```

`NOT NULL DEFAULT 0` is load-bearing. The maintenance is `c = c + $1`, and
`NULL + 1` is `NULL`.

The state table is the framework's. `_autumn_derivations` ships as a framework
migration, folded in automatically when the binary registers at least one
`#[derivation]`. An application with no derivation gets neither the table nor
the boot work.

## What is maintained, and when

Every path below writes the parent inside the mutation's own transaction. If
the derived write fails, the row mutation rolls back with it.

| Operation | Effect on the derived column |
|---|---|
| `save` / `save_many` / `upsert_many` insert | add the new row's contribution |
| `update` with an unchanged foreign key | add the difference between the new and the old contribution |
| `update` that reassigns the foreign key | subtract the old contribution from the old parent, add the new one to the new parent |
| a filter flip on an unchanged parent | add or subtract that one contribution |
| `delete_by_id` / `delete_many` | subtract the row's contribution |
| soft `delete_by_id` | subtract; the row survives and the value reflects live rows |
| `purge` | subtract, and only if the row was still live |
| `restore` | add the contribution back, and only if the row was soft-deleted |
| `dependent` cascades on the parent | as for a counter cache, per affected child |

A row the filter rejects contributes `0`, so inserting, editing or deleting it
issues no statement, and two equal contributions issue none either. The old
contribution is read by SQL before the update, from the row that is about to
change, because that row is gone once the `UPDATE` lands. The arithmetic is one
atomic `UPDATE parents SET col = col + $1`, never a read-modify-write, so N
concurrent inserts yield exactly N. Re-parenting applies its two deltas in
ascending parent id, so two transactions swapping children cannot deadlock.

## Content addressing and backfill

A counter cache is correct from its first day, because the column and the code
ship together. A derivation over an existing table is not: its column has to be
built once, and rebuilt whenever the definition changes.

`DerivationDef::definition_hash` content-addresses the derivation's shape. It is
a SHA-256 over the child table and primary key, the soft-delete flag, the
foreign-key column, the parent table and primary key, the maintained column, the
transform, the lowered filter SQL, the contribution SQL and the tenant column.
Changing the filter, the transform, the column or the foreign key changes the
hash. Renaming the derivation, moving the file or reformatting the filter source
does not, so a cosmetic edit never triggers a backfill.

At startup, after migrations, the framework checks the registry and then calls
`ensure_derivations`. That compares each registered hash with the hash stored in
`_autumn_derivations`. A derivation whose hash matches is left alone, which
keeps a boot from re-backfilling what it already backfilled. A derivation with
no row, or with a different hash, is enqueued as `pending` with its checkpoint
cleared. A derivation whose name has no row carrying its hash, when exactly one
other row does and that row's own name does not claim it, has been renamed: that
row is carried over under the new name, state and checkpoint included, so a
rename really does cost nothing. Rows are matched by hash before names, in two
passes, so two derivations that only swapped names both keep their state, and
the whole reconciliation runs in one transaction holding the state table (an
`EXCLUSIVE` lock on Postgres, so a batch already sweeping finishes first and
reconciliation never deadlocks with it), so replicas booting together take
turns rather than racing each other's renames. The framework then sweeps what was enqueued in a
background task, a few batches per pooled connection. A sharded app reconciles and sweeps on every shard primary as
well as on the control primary.

A registry collision stops the boot, because double counting is data
corruption: two derivations sharing a name, two maintaining one parent column,
or a derivation maintaining a column that something else already maintains: a
plain `counter_cache` on another model, a `#[votable]` model's aggregate
column, a `#[repository(..., position(...))]` ordering column, a model's
`#[lock_version]` token or `tenant_id` discriminator, or a
`#[commentable(counter_cache = ...)]` parent's
count (each registers the column it claims for exactly this check). A database failure does not: it is logged, the sweep for that target
is skipped, and a derivation whose backfill has not run yet is stale rather than
broken, which the actuator reports exactly.

`run_backfill` does one batch per transaction. The batch locks the state row
first, so that row is both the cursor and the mutex: replicas take turns on one
sweep instead of each running their own. It then pages parent ids after the
committed checkpoint, assigns the ground truth to that page and advances the
checkpoint, all in that transaction. The checkpoint therefore never describes a
batch that did not commit, so a killed process resumes from the last committed
one, and the repair assigns rather than adjusts, so re-running a batch is
idempotent anyway. Every state write is guarded by name **and** hash, so a
definition that changed under a running sweep is dropped rather than marked
complete with the old values. Each batch also locks the parents it rebuilds
before it reads their children, so a backfill against live traffic neither
clobbers a committed delta nor reads a half-applied one.

```rust,ignore
use autumn_web::derivation::{BackfillOptions, run_backfill};

// The defaults: batch_size 1000 parents per transaction, max_batches None.
let report = run_backfill(&mut conn, &BackfillOptions::default()).await?;
report.completed;     // names that reached `complete` in this call
report.in_progress;   // names still pending or running, each with its checkpoint
report.rows_repaired; // parent rows actually written
```

`batch_size` bounds how long a batch holds row locks. `max_batches` bounds the
call rather than the sweep, which is how a caller paces a repair by hand: the
next call resumes from the committed checkpoints.

### Changing a definition under a rolling deployment

The delta paths never read the state row: each replica maintains the column
under the definition compiled into it. During a rolling deployment that changes
a filter or a transform, the replicas still on the old binary keep applying
deltas under the old definition while the new binary's backfill assigns the
new one, so a parent an old replica touches after the sweep has passed it is
wrong until it is swept again. The framework cannot see the fleet, so it does
not wait for it. Treat a definition change like a column rename: once no
replica on the old binary is writing, call `derivation::resweep(conn, name)`
(the checkpointed, resumable pass) or `derivation::recompute(conn, name)` (one
synchronous repair) from a post-deploy hook, and the next sweep settles every
parent. Both are idempotent, so running them on a healthy derivation costs one
pass and changes nothing. A one-shot deploy, a single replica, and a change
that only adds a derivation (no old definition to disagree with) need none of
this. A derivation **removed** for a deployment and reinstated later is the
same case in another shape: its state row survives (the status report shows
it as `unregistered` in between), nothing maintained the column while it was
gone, and reconciliation cannot tell a reinstated definition from one that was
never absent, so reinstating it calls for the same `resweep`.

## Status and repair

`GET /actuator/derivations` reports every derivation this binary declares. It is
sensitive-gated, like `/actuator/env`, because the document names parent tables,
child tables and the columns joining them. A process with no database pool
answers `503` rather than `404`.

```json
[
  { "target": "control", "name": "posts.published_comment_count",
    "definition_hash": "3f0a...", "stored_hash": "3f0a...",
    "backfill_state": "complete", "checkpoint": 4200,
    "backfilled_rows": 4200, "updated_at": "2026-09-07 12:00:00+00",
    "drift": 0, "drift_error": null }
]
```

`target` names the database a row was read from: `"control"` for the control
pool, or a shard's name. A sharded app maintains its derivations on every
shard primary, so each shard is reported after the control database, and a
shard that cannot be read contributes one `{ "target": ..., "error": ... }`
row instead of hiding the others. A shard-only deployment (no control role)
reports its shards alone; only a process with no database at all answers
`503`.

`stored_hash` and `backfill_state` are `null` when no state row exists yet.
`backfill_state` is otherwise `pending`, `running` or `complete`, plus a
report-only `unregistered` for a state row this binary declares no derivation
for. Such a row reports `definition_hash: null` and stays in place, because only
an operator can tell a removed derivation from a rolling deploy. `checkpoint`
stays populated after the sweep completes, and `backfilled_rows` counts parents
visited rather than written. `drift: 0` is the healthy answer; a nonzero one
names the derivation to repair. The scan examines at most `DRIFT_SCAN_LIMIT`
(10,000) parents, newest ids first, so it is bounded on a table of any size:
a figure equal to the limit means every parent examined drifted, and drift
confined to older rows beyond that window is not seen by it (`recompute`
repairs the whole table regardless). A scan that could not run reports
`drift: null` with the reason in `drift_error`. That last case
is usually a derived column whose migration has not been applied yet, and the
other derivations are still reported.

```rust,ignore
use autumn_web::derivation::{derivation_status, recompute};

let statuses = derivation_status(&mut conn).await?;
let repaired = recompute(&mut conn, "posts.published_comment_count").await?;
```

`recompute` runs the same batched, lock-then-assign sweep the counter cache
uses. It is idempotent, and it returns how many parent rows it wrote. A healthy
derivation reports `0` and writes nothing. An unregistered name is an error, not
a silent no-op. Each drift figure is one aggregate statement, capped as above,
but it still reads the parent table, so treat `/actuator/derivations` as an
operator endpoint and do not scrape it.

## Testing

A derived value is a column, so the test to write is a query-count assertion:

```rust,ignore
let resp = client.get("/posts").send().await;
resp.assert_ok();
resp.assert_no_n_plus_one();   // or: resp.queries(), resp.assert_max_queries(1)
```

The framework's own evidence is
[`autumn/tests/integration/model_derivation.rs`](../../autumn/tests/integration/model_derivation.rs):
the filtered count and sum, same-transaction rollback, 50 concurrent inserts,
re-parenting, filter flips, hash reconciliation, a killed and resumed backfill,
status, recompute and the query count. CI runs it in the Docker sweep. Set
`AUTUMN_TEST_PG_URL` to run it against a Postgres you already have, instead of a
testcontainer:

<!-- config-key-allow: AUTUMN_TEST_PG_URL — a test-harness variable read by the framework's own suite, not an application config key -->

```console
$ AUTUMN_TEST_PG_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres \
    cargo test -p autumn-web --test integration_tests --features test-support \
    model_derivation -- --ignored
```

<!-- config-key-allow: AUTUMN_TEST_PG_URL — a test-harness variable read by the framework's own suite, not an application config key -->


## Limits

- **One source model.** A derivation folds one child table into one parent
  column. There is no join and no second source. Declare a second derivation
  instead.
- **`i64` columns.** The maintained column is `BIGINT`/`i64`, and a summed field
  must be a non-nullable `i8`, `i16`, `i32` or `i64`.
- **Parent conventions are not compile-checked.** The parent table comes from
  the type name (`Post` gives `posts`), and `parent_table = "..."` overrides it
  for a parent that overrides its own. The parent primary key is always `id`.
  `#[model]` on the child cannot see the parent's fields, so a wrong table name
  or a missing or mistyped column surfaces as a database error on the first
  mutation.
- **A single primary key, and one database.** The child needs a scalar `#[id]`,
  and the parent `UPDATE` runs on the child's connection, so a sharded setup
  must keep parent and child on the same shard.
- **Column names come from field names.** A filter field, summed field or
  `tenant` field renamed with `#[diesel(column_name = "...")]` is rejected. The
  `#[id]` and `fk` fields may be renamed that way: the spec reaches SQL under
  the physical column.
- **A derivation cannot read a column something else maintains on its table.**
  A source (the summed field, or a field in the filter) that another
  derivation, a `counter_cache`, a vote tally or an ordering position maintains
  on the child's table moves under direct SQL, with no delta carrying the change
  up: a comment's `child_score` moves and the post's `sum(child_score)` never
  hears of it. Within one model this is a compile error; across models the
  registry refuses it at boot. The `#[lock_version]` token and the tenant
  discriminator move only under the repository's own hooked paths, so reading
  one is fine. Maintain a multi-level aggregate one level at a time, from the
  leaves up, each as its own derivation over source data.
- **The parent primary key and tenant discriminator are not maintainable
  columns**: `column = "id"` and `column = "tenant_id"` are compile errors, and
  a model's `tenant_id` field also claims its column in the registry, so a
  derivation reaching it by another spelling stops the boot.
- **A self-referential derivation cannot read the column it maintains.** Onto
  its own table, `sum(<the maintained column>)` or a filter naming it is a
  compile error: the parent-side update runs no repository hook, so a row's
  new aggregate would change what it contributes to its own parent without
  that parent being maintained. The same holds for the two columns every
  aggregate reads implicitly: onto its own table a derivation cannot maintain
  its `fk` or its `tenant` column, since a maintained value would re-parent
  the row without carrying its contribution off the old parent.
- **Self-referential derivations sweep one parent per batch.** A child that
  derives onto its own table (a comment's `reply_count`) has rows that are
  children and parents at once, so a batch locking several parents in id order
  could form a lock cycle with a mutation that holds a child row and wants its
  parent. The backfill takes one parent per transaction for such a derivation
  regardless of `batch_size`, `recompute` does the same, and any batch the
  database aborts to break a deadlock is retried. No test in this release
  covers the self-referential shape end to end.
- **Weights are ordinary numbers, not the edges of `i64`.** The delta paths
  never overflow on their own (a difference that does not fit goes out as two
  deltas, a bulk mutation's total is netted in `i128`, and a tenant-scoped
  parent's deltas are ordered so no running sum leaves the span of zero and
  their total), but a parent already sitting at `i64::MIN` or `i64::MAX` that
  receives weights of the same magnitude can still refuse an intermediate
  value, and SQLite's integer `SUM` raises `integer overflow` when a
  scan's running total leaves `i64` even if the final aggregate fits. A
  summed column whose values approach 2^63 is outside what a derivation is
  for; keep weights small enough that any partial sum of them fits.
- **No configuration keys.** Reconciliation and the boot backfill are automatic
  and use the default `BackfillOptions`. Call `run_backfill` to pace a large
  repair by hand.
- **The counter cache's own limits apply**, because the same specs maintain
  both. See [Counter Caches](counter-cache.md) for `upsert_many` under a
  concurrent writer, row-suppressing triggers, tenancy, and the two query
  surfaces that are not yet wired.

## See also

- [Counter Caches](counter-cache.md): the unfiltered sibling, and the shared
  mutation-path contract.
- [`#[votable]`](votable.md): a maintained aggregate over a reaction edge table.
- [Repositories](repositories.md): `dependent`, `soft_delete`, hooks.
