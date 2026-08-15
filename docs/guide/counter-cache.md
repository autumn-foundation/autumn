# Counter Caches — `counter_cache`

Every content app grows the same column: `posts.comment_count`,
`subreddits.subscriber_count`, `teams.member_count`, `pages.revision_count`. It
exists because the honest alternative — a `COUNT(*)` subquery per parent — is an
N+1 across every list view that shows a number.

Keeping that column current by hand is where the bugs live. The increment is
easy and everybody writes it; the **decrement** is the one people forget, so the
count drifts upward forever. The pair is usually not in one transaction, so a
failed insert leaves the count inflated. And `SET c = <value read a moment ago>`
loses updates the instant two people comment at once.

`counter_cache` makes it a declaration:

```rust,ignore
#[autumn_web::model]
#[belongs_to(Post, counter_cache)]
pub struct Comment {
    #[id]
    pub id: i64,
    pub body: String,
    pub post_id: i64,
}
```

That is the whole feature. `PgCommentRepository`'s `save`, `update`,
`delete_by_id`, `restore`, `purge` and every bulk variant now maintain
`posts.comment_count` **inside the same transaction as the row mutation**, with a
single atomic `UPDATE posts SET comment_count = comment_count + $1 WHERE id = $2`.

> A complete runnable version lives in
> [`examples/reddit-clone`](../../examples/reddit-clone): `Comment` carries
> `#[belongs_to(Post, counter_cache)]` in
> [`src/models.rs`](../../examples/reddit-clone/src/models.rs). The framework's
> own [`autumn/tests/integration/model_counter_cache.rs`](../../autumn/tests/integration/model_counter_cache.rs)
> is the canonical evidence — including 50 simultaneous comments on one post —
> and is the suite CI's ignored-test sweep runs on every push.

## The attribute

`counter_cache` is a **`belongs_to`** option. The counter is maintained by the
child's repository — the side that owns the foreign key and runs the
insert/delete — so declaring it on the parent's `#[has_many]` is a directed
compile error that names the leg to move it to.

| Form | Column maintained on the parent |
|---|---|
| `#[belongs_to(Post, counter_cache)]` on `Comment` | `posts.comment_count` |
| `#[belongs_to(Team, counter_cache = "member_count")]` on `Membership` | `teams.member_count` |

The default is `{snake(ChildModel)}_count` — **singular**. Rails pluralises
(`comments_count`); autumn does not, because `#[votable(aggregate = count)]`
already defaults to `{name}_count` and because `posts.comment_count` /
`subreddits.subscriber_count` are the columns this project's own examples have
shipped since their first migration. Name it explicitly when your column differs.

Two conventions are assumed, and both match what `belongs_to` already assumes for
eager loading:

- the **parent table** is derived from the target type (`Post` → `posts`);
- the **parent primary key** is `id`.

## The required migration

The column is yours to create — `counter_cache` maintains a column, it does not
declare DDL (the same contract [`#[votable]`](votable.md) has for its edge table):

```sql
ALTER TABLE posts ADD COLUMN comment_count BIGINT NOT NULL DEFAULT 0;
```

`NOT NULL DEFAULT 0` is load-bearing: the maintenance is `c = c + 1`, and
`NULL + 1` is `NULL`. Scaffolding a counter-cached child emits exactly this —
see [Scaffolding](#scaffolding) below.

Adopting the column on a table that already has rows? Add it, then
[recompute](#repair-and-backfill) once.

## What is maintained, and when

| Operation | Effect |
|---|---|
| `save` / `save_many` / `save_many_skip_invalid` | `+1` per inserted child, per counter-cached leg |
| `update` / `update_many` | foreign key changed → `-1` old parent, `+1` new parent. Unchanged → **no statement at all** |
| `delete_by_id` / `delete_many` | `-1` per removed child |
| `delete_by_id` on a `soft_delete` repository | `-1`; the row survives, the count reflects live rows |
| `restore` | `+1`, and only if the row was actually soft-deleted |
| `purge` | `-1`, and only if the row was still live (a purge after a soft delete does not double-decrement) |
| `upsert_many` | insert → `+1`; update → the before/after diff |
| a parent's `dependent = destroy` cascade | the child's own counters move as each child is destroyed |

A child whose foreign key is `NULL` moves nothing. A leg whose foreign key did
not change issues no statement.

### Same transaction, not "shortly after"

Every one of those runs on the mutation's own connection inside the mutation's
own transaction. Some no-hooks paths are single-statement (and therefore
transaction-free) by design; a counter-cached model opens a transaction on those
paths, and a model without one keeps the exact previous, transaction-free
codegen — the branch is on a `const`, so it is compiled away.

The consequence worth stating plainly: if the counter update fails, the row
creation rolls back with it. The framework's test suite pins this with a parent
whose counter column carries `CHECK (count <= 2)` — the third insert fails on the
*counter*, and the child row is not persisted.

### Atomic, not read-modify-write

The increment is one statement: `SET comment_count = comment_count + $1`. The
database resolves the arithmetic, so N concurrent inserts commute and the result
is exactly N under every interleaving. There is no read-then-write window to
lose, and no row lock is taken on the parent beyond the one the `UPDATE` needs.

## Repair and backfill

Counters drift when something bypasses the repository — a `psql` session, a data
migration, a legacy code path. Every counter-cached repository gets:

```rust,ignore
// Rebuild every parent's counter from the source of truth.
let rows = comments.recompute_counter_caches().await?;

// …or just one parent.
comments.recompute_counter_caches_for(post_id).await?;
```

`recompute` **assigns** a `COUNT(*)`, so it is idempotent by construction: run it
twice, get the same answer. It counts only live rows for a soft-deleting child.
This is both the backfill for a table adopting the column and the repair for
drift, and it is the supported adoption path:

1. `ALTER TABLE posts ADD COLUMN comment_count BIGINT NOT NULL DEFAULT 0;`
2. add `counter_cache` to the child's `#[belongs_to]`;
3. deploy, then call `recompute_counter_caches()` once.

Counters are deliberately **not** clamped at zero. A negative count is a visible
signal that something wrote around the framework; `GREATEST(0, …)` would hide it.
`recompute` is the fix.

## Hand-written inserts

An application that inserts a child with its own SQL, inside its own
transaction, can opt into the same maintenance instead of hand-rolling `count +
1`:

```rust,ignore
let comment_id: i64 = diesel::insert_into(comments::table)
    .values(/* … */)
    .returning(comments::id)
    .get_result(conn)
    .await?;

autumn_web::repository::counter_cache_after_insert_by_id(
    conn,
    Comment::counter_caches(),
    comment_id,
)
.await?;
```

`counter_cache_before_delete_by_id` is the mirror for a hand-written delete (call
it *before* the row goes away). Both take the spec slice explicitly: `#[model]`
emits `counter_caches()` as an **inherent** item shadowing an empty blanket impl,
and an inherent shadow is not visible through a generic trait bound — so the
helpers take the slice rather than recovering it from one.

## Scaffolding

```console
$ autumn generate scaffold comment body:text post:references \
      --belongs-to Post --counter-cache
```

adds, on top of the ordinary `--belongs-to` scaffold:

- `counter_cache` on the generated child's `#[belongs_to(Post, …)]`;
- a migration adding `comment_count BIGINT NOT NULL DEFAULT 0` to `posts`
  (with a `DROP COLUMN` down);
- the column in the parent's `src/schema.rs` block and a `#[default] pub
  comment_count: i64` field on the parent model.

## Limits

- **`belongs_to` only.** Counters over a `through =` join table are rejected at
  compile time: the association's foreign key names a column on the join table,
  not on the child, so the increment would read a column that does not exist. Map
  the join table as its own model and put `counter_cache` on its `belongs_to`.
- **Flat counts.** There is no conditional/filtered counter (Rails'
  `counter_cache` has none either). Count only `published` children by giving
  them their own model or maintaining that column yourself.
- **One column per (parent table, column).** Two counter-cached legs resolving
  onto the same parent column are a compile error — they would both move it and
  double-count. Two legs to *different* parent tables may share a column name.
- **`i64` keys.** The whole surface is typed on `i64` primary and foreign keys,
  like the rest of autumn's repository layer.
- **Same database.** The parent `UPDATE` runs on the child's connection, so a
  sharded setup must keep parent and child on the same shard.

## See also

- [`#[votable]`](votable.md) — the aggregate-column sibling, for signed
  vote scores and unary like counts over a reaction edge table.
- [Repositories](repositories.md) — `dependent`, `soft_delete`, hooks.
