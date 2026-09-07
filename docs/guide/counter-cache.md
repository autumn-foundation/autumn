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

> The framework's own
> [`autumn/tests/integration/model_counter_cache.rs`](../../autumn/tests/integration/model_counter_cache.rs)
> is the canonical evidence — including 50 simultaneous comments on one post —
> and is the suite CI's ignored-test sweep runs on every push.
>
> `examples/reddit-clone` used to carry the worked example here, on a
> `Comment` model that `belongs_to` a `Post`. Comments there are now a
> *polymorphic* association (#1367), which keeps `posts.comment_count` current
> through this very mechanism but declares it with `#[commentable]` rather than
> `#[belongs_to(..., counter_cache)]` — see
> [Threaded Comments on Anything](commentable.md).

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

Two conventions are assumed about the **parent**, and both match what
`belongs_to` already assumes for eager loading:

- the **parent table** is derived from the target type (`Post` → `posts`);
- the **parent primary key** is `id`.

Neither is checked at compile time — `#[model]` on the child cannot see the
parent's fields, so a parent that overrides its table (`#[model(table =
"person")]`) or keys on something other than `id` produces SQL that fails at
runtime (`relation "people" does not exist`) rather than a compile error. The
same is true of a typo in `counter_cache = "…"`. Those are the parent-side
conventions to check when adopting the attribute; the child side (its table, its
primary key, its foreign key) is all resolved from the model the attribute sits
on and cannot drift.

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
| a parent's `dependent = delete_all` cascade | `-1` on **every** counter-cached leg — the child rows go away entirely |
| a parent's `dependent = nullify` cascade | `-1` on **only** the leg being cleared |

A child whose foreign key is `NULL` moves nothing. A leg whose foreign key did
not change issues no statement.

The two bulk cascades resolve the affected child ids *before* their statement
runs, because a `nullify` is about to overwrite the very foreign key that
identifies them. `nullify` is deliberately narrower than `delete_all`: the
children survive the detach, so their other counter-cached legs still hold them
and decrementing every leg would permanently undercount those parents —
clearing `comments.author_id` when a user is deleted must not drop
`posts.comment_count` for comments still attached to their post.

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

### Locking and races

Reading a child's current parent and writing its new one is a read-then-write, so
the read takes a row lock on the **child** (`SELECT … FOR UPDATE` on Postgres;
`SQLite` needs none — generated write paths open with `BEGIN IMMEDIATE`, which
excludes every other writer). It is the same row the surrounding mutation locks,
so no new lock-ordering edge appears. When a foreign key moves, the two parent
deltas are applied in **ascending parent id**, not old-then-new, so two
transactions swapping children between the same pair of parents cannot deadlock.

One narrow window is left open by design: `delete_by_id` resolves the parent from
the child row in the decrementing statement and removes the row in the next one.
A concurrent transaction that re-parents that child between the two would leave
the counter one off. It is a genuinely rare interleaving (deleting and
re-parenting the same row concurrently), and `recompute` is the repair.

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

You do not have to take the application down to do it. Safety against live
traffic is not free, though: a repair that simply counted would *introduce*
drift, because a child insert that has already taken the parent's row lock but
has not committed is invisible to the repair's snapshot, so the repair would
overwrite that increment the moment it commits. Each batch therefore locks the
parents it is about to rebuild in a separate, earlier statement, which forces
the repair and the increment into a definite order either way round. The sweep
runs in batches of 1,000 parents, one short transaction each, so it never holds
locks over the whole table.

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
it *before* the row goes away). Both resolve the parent from the child row
through a sub-select, so they work without the caller knowing which legs are
counter-cached; where the parent id is already in hand and that lookup matters,
`counter_cache_apply_delta(conn, spec, parent_id, delta, scope)` takes it
directly (pass `TenantScope::SameTenantAsChild(child_id)`, or
`TenantScope::Unscoped` when the association declares no tenant column). Both take the spec slice explicitly: `#[model]`
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
  (with a `DROP COLUMN` down).

The parent's `src/schema.rs` block and model struct still need the column, and
the scaffold prints the two exact lines rather than editing them. That is
deliberate: they are files this invocation does not own, and neither edit has a
marker-delimited revert, so writing them would leave `autumn destroy scaffold`
unable to take them back out. Until you add them the counter is maintained (it
is raw SQL) but not readable from Rust.

## Limits

- **`belongs_to` only.** Counters over a `through =` join table are rejected at
  compile time: the association's foreign key names a column on the join table,
  not on the child, so the increment would read a column that does not exist. Map
  the join table as its own model and put `counter_cache` on its `belongs_to`.
- **Flat counts.** `counter_cache` itself counts every live child, with no
  predicate and no weight. To count only `published` children, or to sum a
  field over them, declare a [`#[derivation]`](derivations.md) instead.
- **One column per (parent table, column).** Two counter-cached legs resolving
  onto the same parent column are a compile error — they would both move it and
  double-count. Two legs to *different* parent tables may share a column name.
  The check keys on the *convention-derived* parent table, so two legs to two
  type names that a `table = "…"` override collapses onto one physical table are
  not caught.
- **A single primary key.** The maintenance addresses the child by one id, so a
  composite `#[id]` is a compile error rather than a decrement keyed on the first
  component.
- **`i64` keys.** The whole surface is typed on `i64` primary and foreign keys,
  like the rest of autumn's repository layer.
- **Same database.** The parent `UPDATE` runs on the child's connection, so a
  sharded setup must keep parent and child on the same shard.
- **Tenancy follows the foreign key.** The parent `UPDATE` is by primary key with
  no tenant predicate: it moves the counter on exactly the row the child's own
  foreign key names. On a `tenant_scoped` repository that means a child written
  with a foreign key pointing at another tenant's parent will move that parent's
  counter — but that child row is itself already cross-tenant, which is the
  problem to fix. Validate the foreign key (a `before_create` hook, or a
  composite `(id, tenant_id)` foreign key in the schema) if a caller can supply
  it directly.
- **The column is yours.** The attribute maintains a column; it does not create
  one, and it does not verify at compile time that the parent has it. A missing
  or mistyped column surfaces as a database error on the first mutation, not as
  silence.
- **`upsert_many` on Postgres can miscount a row raced in by another writer.**
  The upsert classifies each row as an insert or an update from the snapshot it
  loaded `FOR UPDATE` beforehand, so every row that already existed is locked
  and its diff is exact. A row that did *not* exist then — nothing to lock — and
  that another transaction inserts before the `INSERT … ON CONFLICT` runs is
  updated rather than inserted, but still counted as an insert: the `+1`
  duplicates the other transaction's, and a foreign key moved by the same upsert
  loses its old parent's decrement. It needs the same primary key to be written
  concurrently by two paths. `SQLite` is unaffected (`BEGIN IMMEDIATE` excludes
  other writers), and `recompute` is the repair.
- **A row-suppressing `BEFORE` trigger will drift the counter.** The bulk
  decrement is computed from the same filters the `DELETE`/`UPDATE` applies, so
  a soft-deleted, cross-tenant or already-gone row is excluded from both. What
  it cannot see is a hand-written `BEFORE DELETE` / `BEFORE UPDATE` trigger that
  returns `NULL` for some rows: the statement succeeds, those children stay
  live, and their parents end up undercounted. Nothing else in the framework
  models a trigger that vetoes the mutation it was handed either — `recompute`
  is the repair.
- **Two opt-in query surfaces are not yet wired**, and will drift until
  `recompute` runs: a derived `fn delete_by_<field>(…)` declared on the
  repository trait, and `find_or_create_by_<field>(…)`. Both are explicit
  declarations rather than part of the default CRUD surface, so a repository
  that does not declare them is unaffected. If you declare either on a
  counter-cached model, schedule `recompute_counter_caches()` or move the call
  through `save` / `delete_by_id`.

## Tenant scoping

A counter update writes a parent row the caller only had to name the **id** of.
On a shared multi-tenant table that is a cross-tenant write waiting to happen: a
child inserted with a foreign key pointing at another tenant's parent moves that
parent's counter, because nothing in a plain `REFERENCES` constraint checks
`tenant_id`.

Name the column and the framework confines every delta to the caller's tenant:

```rust,ignore
#[belongs_to(Post, counter_cache, counter_cache_tenant = "tenant_id")]
```

Both tables must carry a column by that name — the predicate is
`posts.tenant_id = comments.tenant_id`, i.e. "the parent sits in the same tenant
as the child that named it". It is explicit rather than inferred because
`#[model]` on the child cannot see the parent's fields, and guessing would turn
every tenant-scoped child hanging off a *global* parent into a hard `column
"tenant_id" does not exist`.

Without the key, no predicate is emitted anywhere and the SQL is exactly what a
single-tenant app would get.

`recompute` honours the same predicate: its ground-truth `COUNT(*)` excludes
cross-tenant children, so a repair sweep cannot undo the isolation that every
ordinary delta enforces. It still writes across tenants in the sense that it
repairs every parent row — it is an operator-level repair, not a request-scoped
one — and it is rejected under `across_tenants()` on a sharded repository, where
it could only reach one shard.

One case the predicate does **not** cover: changing a child's tenant
discriminator itself (only reachable through `across_tenants()`). The
maintenance keys on the foreign key, so a row that moves tenant without changing
parent stops or starts satisfying the predicate with no delta issued, and a
simultaneous re-parent scopes the old parent's decrement by the child's *new*
tenant. Moving rows between tenants is a data-migration-shaped operation —
run `recompute_counter_caches()` afterwards.

Bulk paths do **not** fold a tenant-scoped batch into one `UPDATE` per parent the
way an unscoped one does: the predicate is anchored to a single child row, so
collapsing a mixed-tenant batch behind one arbitrary witness would either sweep
cross-tenant children into the increment or drop legitimate ones. Tenant-scoped
associations therefore trade the folding optimization for exactness.

## Filtered and weighted counts

`counter_cache` counts every live child. For a count restricted by a predicate,
or a weighted `sum(field)` over the qualifying rows, declare a
[`#[derivation]`](derivations.md) on the child instead:
`#[derivation(Post, column = "published_comment_count", filter = published)]`.
It is maintained by the same mutation paths and the same transaction contract
described above, and it adds a content-addressed definition hash, a resumable
backfill and `/actuator/derivations` for state and drift. See
[Maintained Derived Read Models](derivations.md).

## See also

- [Maintained Derived Read Models](derivations.md) — `#[derivation]`, the
  filtered and weighted superset of this attribute.
- [`#[votable]`](votable.md) — the aggregate-column sibling, for signed
  vote scores and unary like counts over a reaction edge table.
- [Repositories](repositories.md) — `dependent`, `soft_delete`, hooks.
