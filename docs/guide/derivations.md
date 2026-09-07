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
| `fk` | the `#[belongs_to]` leg to that parent, else `{snake(Parent)}_id` | the child column naming the parent |
| `tenant` | none | tenant-discriminator column, as `counter_cache_tenant` |
| `name` | `{parent_table}.{column}` | the registry name, used by the state table and the actuator |

`sum(<field>)` requires a non-nullable integer field (`i8`, `i16`, `i32` or
`i64`). A nullable or floating-point sum is a compile error, because the Rust
and the SQL lowering would disagree on it.

Two derivations that maintain one `(parent table, column)` pair are a compile
error, as is a derivation colliding with a counter cache on the same model: both
would move the column twice. Two derivations sharing a `name` are a startup
error, because they would share one state row.

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
to `is_some_and` rather than `!=`. A plain `!=` in Rust would count the NULL row
that SQL excludes, and the two lowerings would drift apart.

A string literal is single-quoted for SQL, and an embedded `'` is doubled. A
literal `{` or `}` in a string is rejected, because it could forge the `{c}`
placeholder. Ordering comparisons on a string field are rejected too: Rust
compares bytes and SQL compares by collation, so `status > "b"` would mean two
different things in the two lowerings. Compare a string with `==` or `!=`.

Everything else is a compile error whose message lists the grammar: `||`,
arithmetic, any method call other than the two NULL probes, float literals, a
name that is not a field, and a field of an unsupported type.

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
issues no statement. Two equal contributions issue none either. The old
contribution is read by SQL before the update, from the row that is about to
change, because that row is gone once the `UPDATE` lands. The arithmetic is one
atomic `UPDATE parents SET col = col + $1`, never a read-modify-write, so N
concurrent inserts yield exactly N. Re-parenting applies its two deltas in
ascending parent id, so two transactions swapping children between the same
parents cannot deadlock.

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

At startup, after migrations, the framework calls `ensure_derivations`. That
compares each registered hash with the hash stored in `_autumn_derivations`. A
derivation whose hash matches is left alone, which keeps a boot from
re-backfilling what it already backfilled. A derivation with no row, or with a
different hash, is enqueued as `pending` with its checkpoint cleared. The
framework then spawns `run_backfill` in the background with default options.
Failures there are logged and swallowed: a derivation whose backfill has not run
yet is stale, not broken, and the actuator reports exactly that.

`run_backfill` pages parent ids outside any transaction, then repairs one page
and advances that derivation's checkpoint inside one transaction. The checkpoint
can therefore never describe a batch that did not commit, so a killed process
resumes from the last committed batch. The repair assigns the ground truth
rather than adjusting it, so re-running a batch is idempotent anyway. Each batch
also locks the parents it rebuilds before it reads their children, so a backfill
against live traffic neither clobbers a committed delta nor reads a half-applied
one.

```rust,ignore
use autumn_web::derivation::{BackfillOptions, run_backfill};

// The defaults: batch_size 1000 parents per transaction, max_batches None.
let report = run_backfill(&mut conn, &BackfillOptions::default()).await?;
report.completed;     // names that reached `complete` in this call
report.in_progress;   // names still `running`, each keeping its checkpoint
report.rows_repaired; // parent rows actually written
```

`batch_size` bounds how long a batch holds row locks. `max_batches` stops the
sweep early, which is how a caller paces a repair by hand.

## Status and repair

`GET /actuator/derivations` reports every derivation this binary declares. It is
sensitive-gated, like `/actuator/env`, because the document names parent tables,
child tables and the columns joining them. A process with no database pool
answers `503` rather than `404`.

```json
[
  { "name": "posts.published_comment_count",
    "definition_hash": "3f0a...", "stored_hash": "3f0a...",
    "backfill_state": "complete", "checkpoint": null,
    "backfilled_rows": 5, "updated_at": "2026-09-07 12:00:00+00",
    "drift": 0 }
]
```

`stored_hash` and `backfill_state` are `null` when no state row exists yet.
`backfill_state` is otherwise `pending`, `running` or `complete`. `drift: 0` on
every row is the healthy answer. A nonzero one names the derivation to repair:

```rust,ignore
use autumn_web::derivation::{derivation_status, recompute};

let statuses = derivation_status(&mut conn).await?;
let repaired = recompute(&mut conn, "posts.published_comment_count").await?;
```

`recompute` runs the same batched, lock-then-assign sweep the counter cache
uses. It is idempotent, and it returns how many parent rows it wrote. A healthy
derivation reports `0` and writes nothing. An unregistered name is an error, not
a silent no-op. Each drift figure is one aggregate statement, but that statement
is a full scan of the parent table, so treat `/actuator/derivations` as an
operator endpoint and do not scrape it.

## Testing

A derived value is a column, so the test to write is a query-count assertion:

```rust,ignore
let resp = client.get("/posts").send().await;
resp.assert_ok();
resp.assert_no_n_plus_one();   // or: resp.queries(), resp.assert_max_queries(1)
```

The framework's own evidence is
[`autumn/tests/integration/model_derivation.rs`](../../autumn/tests/integration/model_derivation.rs).
It covers the filtered count and sum, same-transaction rollback, 50 concurrent
inserts, re-parenting, filter flips, hash reconciliation, a killed and resumed
backfill, status, recompute and the query count. CI runs it in the Docker sweep.
Set `AUTUMN_TEST_PG_URL` to run it against a Postgres you already have, instead
of a testcontainer:

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
  the type name (`Post` gives `posts`) and the parent primary key is `id`.
  `#[model]` on the child cannot see the parent's fields, so a parent `table =
  "..."` override, or a missing or mistyped column, surfaces as a database error
  on the first mutation.
- **A single primary key, and one database.** The child needs a scalar `#[id]`,
  and the parent `UPDATE` runs on the child's connection, so a sharded setup
  must keep parent and child on the same shard.
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
