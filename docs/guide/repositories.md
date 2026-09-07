# Repositories & Bulk Operations

Repositories in `autumn-web` provide a clean, type-safe, and highly optimized ORM-like data access layer. By annotating a trait with `#[autumn_web::repository(Model, table = "table_name")]`, Autumn automatically generates high-performance implementations targeting PostgreSQL using `diesel-async`.

In version `0.5.0`, Autumn introduces high-performance **Bulk CRUD operations** to minimize database round trips and execute massive writes transaction-safely and hook-compliantly.

---

## Generated Bulk CRUD Methods

When you declare a repository, the generated `Pg[Name]Repository` automatically implements the following high-performance bulk operations:

```rust
fn save_many(
    &self, 
    new: &[NewModel]
) -> impl Future<Output = AutumnResult<Vec<Model>>> + Send;

fn save_many_skip_invalid(
    &self, 
    new: &[NewModel]
) -> impl Future<Output = AutumnResult<(Vec<Model>, Vec<(usize, AutumnError)>)>> + Send;

fn update_many(
    &self, 
    ids: &[i64], 
    changes: &UpdateModel
) -> impl Future<Output = AutumnResult<Vec<Model>>> + Send;

fn delete_many(
    &self, 
    ids: &[i64]
) -> impl Future<Output = AutumnResult<()>> + Send;

fn upsert_many(
    &self, 
    records: &[Model]
) -> impl Future<Output = AutumnResult<Vec<Model>>> + Send;
```

---

## 1. High-Performance Batch Insertion: `save_many`

`save_many` takes a slice of new records and inserts them in a single batch statement.

### Non-Hooked (Zero-Cost Path)
If your model has no hooks configured, `save_many` translates to a single SQL query:
```sql
INSERT INTO table_name (col1, col2, ...) 
VALUES ($1, $2, ...), ($3, $4, ...), ... 
RETURNING *;
```
For large inputs, queries are automatically chunked under the Postgres parameter ceiling (65,535 parameters), preventing compilation or runtime DB overflow errors.

### Hook-Aware Execution
If hooks are enabled on your repository, `save_many` guarantees full transaction integrity:
1. Runs the model's `#[validate]` rules on every record, before any record is written — one offender aborts the batch with a 422.
2. Runs `before_create` hooks **sequentially** on each record.
3. Batches the validated records and inserts them in a single database round trip inside a transaction.
4. Runs `after_create` hooks sequentially on successfully inserted records.
5. Stages `after_create_commit` hooks to fire only after the surrounding transaction successfully commits.

---

## 2. Validation & Partial Success: `save_many_skip_invalid`

When bulk importing dirty external data (e.g., from CSVs or public API hooks), some rows might violate business rules or database constraints. `save_many_skip_invalid` enables maximum throughput without losing valid rows.

- It runs the model's `#[validate]` rules on each row first, then the `before_create` hooks, and filters out the failures of either.
- A rejected row is reported against **the caller's own index**, so a `(index, error)` pair still points at the CSV line it came from.
- It attempts a high-speed batch insert of all successful records in a transaction.
- **Constraint Fallback**: If the batch insert fails due to a database constraint (e.g., `UniqueViolation`), it automatically falls back to row-by-row insertion for that chunk, isolating individual DB constraint failures.
- Returns a tuple of `(successful_models, list_of_errors_with_indices)`.

---

## 3. Bulk Updates: `update_many`

`update_many` modifies a batch of records identified by their IDs in a single SQL operation.

### Non-Hooked
Updates all matching rows directly:
```rust
repo.update_many(&[1, 2, 3], &UpdatePost { title: Some("Bulk Updated Title".to_string()) }).await?;
```

### Hook-Aware
If `before_update` hooks are configured:
1. Performs a `SELECT ... FOR UPDATE` on all specified IDs to load their current state.
2. For each row, constructs an `UpdateDraft` containing the original model and applies the changes.
3. Runs `before_update` hooks on each draft.
4. Updates all matching records in the database.
5. Runs `after_update` hooks.

---

## 4. Bulk Deletions: `delete_many`

`delete_many` deletes or soft-deletes a batch of records in a single statement.

### Non-Hooked
Runs a single direct delete or soft-delete update statement.

### Hook-Aware
1. Performs a `SELECT ... FOR UPDATE` on all specified IDs.
2. Runs `before_delete` hooks sequentially.
3. Executes the batch delete / soft-delete.
4. Runs `after_delete` hooks sequentially.

---

## 5. Bulk Upserts: `upsert_many`

`upsert_many` executes high-performance "insert-or-update" operations using a single SQL query matching on the primary key:
```sql
INSERT INTO table_name (id, col1, col2, ...) 
VALUES ($1, $2, ...), ($3, $4, ...) 
ON CONFLICT (id) DO UPDATE SET col1 = EXCLUDED.col1, ... 
RETURNING *;
```

> [!IMPORTANT]
> **Compile-Time Hook Safety**: If hooks are enabled on your repository, calling `upsert_many` is explicitly **rejected at compile-time**. 
> Because Postgres determines whether a row will insert vs update at runtime, it is impossible to correctly invoke `before_create` or `before_update` hooks before sending the query. To prevent silent hook bypass, this is caught during compilation.

---

## 6. Race-safe get-or-insert: `find_or_create_by_<field>` *(unreleased)*

The classic "find it, and if it isn't there create it" pattern is a **TOCTOU
race**: between your `find_by_slug` returning empty and your `insert` landing,
another request can insert the same key — and one of you gets a Postgres
`23505` unique-violation (which also aborts the surrounding transaction).

Declare the lookup in the repository trait (just the lookup fields — the `new`
value is added for you):

```rust
#[autumn_web::repository(Subreddit)]
pub trait SubredditRepository {
    /// Backed by `CREATE UNIQUE INDEX ... ON subreddits (slug)`.
    fn find_or_create_by_slug(slug: String);
}
```

This generates an inherent method on `PgSubredditRepository`:

```rust
pub async fn find_or_create_by_slug(
    &self,
    slug: String,
    new: &NewSubreddit,
) -> AutumnResult<(Subreddit, bool)>;
```

Call it and branch on the returned `created` flag if you care:

```rust
let (community, created) = repo
    .find_or_create_by_slug(slug.clone(), &new_sub)
    .await?;
if created {
    tracing::info!(%slug, "created new community");
}
```

Composite keys use `_and_`, matching a **composite** unique index:

```rust
// UNIQUE (user_id, list_id)
fn find_or_create_by_user_id_and_list_id(user_id: i64, list_id: i64);
```

### The `(Model, bool)` return

The method returns `(model, created)`. `created == true` **only** when this call
actually inserted the row. When a matching row already exists — or when a
concurrent caller won the insert race — you get the existing row with
`created == false`.

### How it stays race-safe

1. **Preliminary lookup** on the read path (replica-eligible, honoring tenant
   scoping and soft-delete). A `#[normalize]` lookup column is canonicalized
   first, so the lookup matches the value step 2 would store. A hit returns
   `(row, false)` immediately and fires **no** hooks.
2. Otherwise the payload is **normalized and validated** — the create half is an
   insert, so the model's rules apply (#2586). A refusal does not overtake the
   found path: because step 1 is replica-eligible and a concurrent caller may
   have inserted since, the method re-checks the primary and returns that row if
   it exists, rather than letting replication lag decide between `(row, false)`
   and a 422.
3. Then **insert on the primary** with `INSERT ... ON CONFLICT DO NOTHING`.
   `ON CONFLICT DO NOTHING` is the crux: instead of raising `23505` (and
   poisoning the transaction), Postgres silently skips a conflicting insert.
   - If a row comes back, this call created it → `(row, true)`, and create/commit
     hooks fire.
   - If nothing comes back, a concurrent caller won → the method re-reads the
     row **on the primary** (read-your-writes) and returns `(row, false)` with no
     hooks.

Under 10+ concurrent callers for the same key, exactly one row exists
afterward, exactly one caller observes `created == true`, and **no
unique-violation ever surfaces to any caller.**

### Hooks and replica routing

- Lifecycle hooks (`before_create` / `after_create` and the durable
  `after_create_commit` commit-hook queue) fire **only on the created path** —
  never when the preliminary lookup finds an existing row.
- One caveat: `before_create` runs *before* the `ON CONFLICT DO NOTHING` insert,
  so when a concurrent caller wins the insert race the loser's `before_create`
  has already executed — and any DB writes it made inside the transaction still
  commit even though that caller's row ends up *not* created (it returns
  `created == false`). Only `after_create` and the `after_create_commit` commit
  hooks are guaranteed to run **exclusively** on the created path. Keep
  `before_create` side effects idempotent, or move create-only work into
  `after_create`.
- Unlike `upsert_many`, `find_or_create_by_*` **is** generated on repositories
  that configure `hooks = ...`. Because the found-vs-created decision is made
  before any hook runs, there is no hook-bypass hazard.
- The lookup may run on a replica; the insert and the read-your-writes re-lookup
  always run on the primary, consistent with `on_primary()` write routing.

### You must have a unique constraint (AC6)

**Race-safety depends entirely on a unique constraint (or unique index)
covering the lookup column(s).** `ON CONFLICT DO NOTHING` only skips inserts
that violate a constraint — with no matching constraint, two concurrent callers
will each insert a row and you get duplicates. The method cannot detect a
missing constraint at compile time, so this is on you:

- Single-field `find_or_create_by_slug` → `UNIQUE (slug)`.
- Composite `find_or_create_by_a_and_b` → `UNIQUE (a, b)`.
- On a `tenant_scoped` repository the unique index should include `tenant_id`
  (e.g. `UNIQUE (tenant_id, slug)`) so the constraint and the tenant-filtered
  re-lookup agree.
- On a `soft_delete` repository the unique constraint **must be a partial index
  scoped `WHERE deleted_at IS NULL`** (e.g.
  `CREATE UNIQUE INDEX ... ON subreddits (slug) WHERE deleted_at IS NULL`).
  With a plain (non-partial) unique index, a soft-deleted row keeps occupying
  the unique slot: the insert conflicts with it, while the `deleted_at IS NULL`
  lookup can't see it — so the re-lookup finds nothing and the method returns
  the internal error below. A partial index frees the slot the moment a row is
  soft-deleted, keeping the constraint and the filtered lookup in agreement.

If an insert conflicts but the follow-up re-lookup finds nothing — the tell-tale
sign that the conflict fired on a *different* constraint than the one you're
looking up by — the method returns a clear internal error rather than silently
looping or lying. Only `_and_` composites are supported; `_or_` is **rejected**
at compile time because it would span multiple constraints and defeat the
single-constraint guarantee.

---

## 7. Grouped aggregate queries: `count_/sum_/avg_/min_/max_..._grouped_by_<col>` *(unreleased)*

Dashboard roll-ups — a post's vote tally, an experiment's audit-trail size, a
per-day event time series — are `GROUP BY` aggregates. Hand-writing them as raw
`diesel::sql_query("SELECT … SUM(...) … GROUP BY …")` strings bypasses the
repository's replica routing, tenant scoping, and soft-delete filters, and has
to be re-typed for every widening cast. Declare them on the `#[repository]`
trait instead (issue #1364):

```rust
#[autumn_web::repository(Vote, table = "votes")]
pub trait VoteRepository {
    /// COUNT(*)  GROUP BY post_id → Vec<(post_id, count)>.
    fn count_grouped_by_post_id() -> Vec<(i64, i64)>;
    /// SUM(value) GROUP BY post_id → Vec<(post_id, Option<sum>)>.
    fn sum_value_grouped_by_post_id() -> Vec<(i64, Option<i64>)>;
    /// AVG(value) GROUP BY post_id → Vec<(post_id, Option<f64>)>.
    fn avg_value_grouped_by_post_id() -> Vec<(i64, Option<f64>)>;
}
```

Each declared method becomes an **inherent** method on the generated `Pg*`
struct that returns a lazy `GroupedAggregate<'_, K, V>` builder — nothing runs
until the terminal `.load().await`:

```rust
// Top-5 posts by score, highest first.
let top: Vec<(i64, Option<i64>)> = repo
    .sum_value_grouped_by_post_id()
    .order_by_aggregate_desc()
    .limit(5)
    .load()
    .await?;

// One post's tally: group by post_id, scope to it, take the single row.
let score = repo
    .sum_value_grouped_by_post_id()
    .filter_eq(post_id)
    .load()
    .await?
    .into_iter()
    .next()
    .and_then(|(_, sum)| sum)
    .unwrap_or(0);

// A day-bucketed time series over a bounded window.
use autumn_web::aggregate::DateBucket;
let per_day: Vec<(chrono::NaiveDateTime, i64)> = repo
    .count_grouped_by_created_at()
    .bucket(DateBucket::Day)
    .filter_range(window_start, window_end)
    .load()
    .await?;
```

### Method-name shapes and the `Vec<(K, V)>` return

The trait method **must** declare its pair return type; the macro reads `K` and
`V` from it and bakes the matching Postgres bind/result SQL types.

| method shape                              | `V`                                |
|-------------------------------------------|------------------------------------|
| `count_grouped_by_<col>`                  | `i64`                              |
| `sum_<num_col>_grouped_by_<col>`          | `Option<T>` (`T` = column type)    |
| `min_/max_<num_col>_grouped_by_<col>`     | `Option<T>`                        |
| `avg_<num_col>_grouped_by_<col>`          | `Option<f64>`                      |

`K` is the group column's Rust type (or, under `.bucket(..)`, the bucket-start
timestamp's type). `sum`/`min`/`max`/`avg` are **null-safe**: a group whose
values are all `NULL` yields `None`, and an empty result set is an empty `Vec`.

A nullable group-key **type** (`K = Option<T>`) is unsupported and rejected at
compile time. A nullable group-key **column** is safe: rows whose group key is
`NULL` are silently **excluded** from the results (the generated query guards the
group column with `IS NOT NULL`), so the `NULL` group is omitted rather than
deserialized into the non-nullable `K`. Nullable **value** columns are fine — an
all-`NULL` group simply yields `(key, None)`.

Grouped aggregates are **not** available on an `#[encrypted(...)]` column (as the
group key or as an aggregated value): the stored value is ciphertext, so grouping
would return ciphertext keys and `.filter_eq(..)` would compare plaintext against
ciphertext and match nothing. Such a method returns an error at call time — use a
raw query, or group on a non-encrypted column.

### Builder chain

- `.order_by_aggregate_desc()` / `.order_by_aggregate_asc()` — order by the
  aggregated value; combine with `.limit(n)` for a top-N roll-up.
- `.limit(n)` — cap the number of groups returned.
- `.filter_eq(v)` / `.filter_range(lo, hi)` — scope rows **before** grouping;
  both filter the *raw group column* and are bound as query parameters (never
  interpolated). `filter_range` is inclusive and works for date/time windows.
  Note they filter the **raw** column even under `.bucket(..)`, so to window a
  bucketed time series pass the raw-timestamp range to `.filter_range(lo, hi)` —
  `.filter_eq(bucket_start)` would match only rows on the exact bucket boundary.
- `.bucket(DateBucket::{Day, Week, Month})` — group by
  `date_trunc('<unit>', <col>)`, producing a time series keyed by bucket start.
  This method is **only available when the group column is a timestamp type**
  (`NaiveDateTime` or `DateTime<Utc>`); non-temporal group keys (e.g. an `i64`
  `post_id`) have no `.bucket()` method, so an invalid `date_trunc` over a
  non-timestamp column is a compile error rather than a runtime failure.
  The truncation zone follows the key type: a `NaiveDateTime`
  (`timestamp WITHOUT time zone`) bucket truncates on the **stored wall-clock**
  value (a deterministic field truncation, independent of the session
  `TimeZone`), while a `DateTime<Utc>` (`timestamptz`) bucket is computed **in
  UTC** — the generated SQL uses `date_trunc('<unit>', <col>, 'UTC')` so bucket
  boundaries stay stable across deployments regardless of the DB session zone,
  consistent with the `DateTime<Utc>` key type.

### Scoping comes for free

The generated query composes the repository's soft-delete filter and tenant
predicate exactly like `count`, and acquires its connection through the same
read-route helper — so **replica routing and multi-tenancy work with no extra
code**. Because `sum`/`avg`/`min`/`max` cannot be merged across shards, a
sharded, tenant-scoped repository used via `across_tenants()` **rejects** a
grouped aggregate rather than returning a per-shard-partial answer; run it per
shard with `from_shard(..)` instead.

---

## 8. Declarative reactions: `#[votable]` + `react()`/`reaction_of()` *(unreleased)*

Votes, likes, and favourites are the same shape every time: a
`(reactor, target)`-unique edge table, a toggle/flip/insert on it, and a
denormalised `score` / `{x}_count` on the target that has to stay exactly equal
to `SUM(value)` / `COUNT(*)`. Hand-written, that is a read-then-write race on
the edge *and* a lost-update race on the aggregate whenever two different
reactors touch the same target.

Declare it on the `#[model]` instead (issue #1362):

```rust
#[autumn_web::model]
#[votable(by = User, aggregate = sum)]   // must be BELOW #[model]
pub struct Post {
    #[id]
    pub id: i64,             // must be i64
    pub score: i64,          // the aggregate column, must be i64
}
```

Any `#[repository]` for that model then picks up two helpers from the emitted
`{Model}Reactions` trait — no repository attribute needed:

```rust,ignore
use crate::models::PostReactions as _;

let r = posts.react(user_id, post_id, 1).await?;  // count mode: no `value` arg
r.value;      // Option<i16> — this reactor's reaction after the call
r.aggregate;  // i64 — the newly persisted score, exact as of commit
r.outcome;    // Inserted | Flipped | Removed

let mine: Option<i16> = posts.reaction_of(user_id, post_id).await?;
```

`react()` is a race-safe toggle: the same value again toggles the edge off, a
different value flips it, a new one inserts it — and the aggregate is
recomputed from ground truth and persisted **in the same transaction**, under a
`FOR NO KEY UPDATE` lock on the target row (weak enough not to block
referencing inserts such as a new comment on the same post), so a reader never
observes edge/aggregate disagreement. It is *not* idempotent: never blindly
retry a timed-out call, or the retry toggles the reaction back off. It also
does **not** validate `value` — put a `CHECK` on the column and never bind
`value` from a request. Soft-deleted targets are `NotFound`. Like the m2m
mutation helpers, `react()` acquires **its own** pooled connection and does not
join an enclosing `Db::tx` — do not hold a `Db` extractor across the call, or
the handler needs two connections at once and deadlocks once concurrency
reaches the pool size. `reaction_of()` is a read: it routes through the
repository's read route, so it is replica-eligible and does not pin
read-your-writes.

The matching no-JS htmx widget is `autumn_web::widgets::reaction_controls`.
Full treatment — the defaults table, the required migration, the before/after
that deletes reddit-clone's hand-written vote SQL, the race-safety proof, and
the known limits — is in the [votable guide](votable.md).

---

## 9. Dependent cascades: `dependent = <action>`

Deleting a parent row usually has to do something about its children. Autumn
generates that cascade for you, transactionally, from a declaration — you never
hand-write the child SQL or a `Db::tx` wrapper.

### Two places to declare it

**On the model (preferred).** Put the action on the `#[has_many]` / `#[has_one]`
leg that already describes the association:

```rust
#[autumn_web::model(table = "posts")]
#[has_many(Comment,  dependent = destroy)]
#[has_many(Vote,     dependent = delete_all)]
#[has_many(Bookmark, dependent = nullify)]   // Bookmark::post_id must be Option<i64>
#[has_many(Award,    dependent = restrict)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
}

#[autumn_web::repository(Post, table = "posts")]
pub trait PostRepository {}
```

`on_delete = <action>` is accepted as an exact synonym of `dependent = <action>`,
so the two declaration sites can be spelled identically.

This form resolves each child's repository by the **`Pg{Child}Repository` naming
convention** (`Comment` → `PgCommentRepository`), which is what
`#[repository(...)]` on a `CommentRepository` trait generates. The name is
resolved unqualified at the model's definition site, so the child repository
must be nameable there.

**On the repository (escape hatch).** When the child's repository does not follow
that convention — a hand-named type, or one imported from another crate — name it
explicitly:

```rust
#[autumn_web::repository(
    Post,
    dependent(PgCommentRepository, fk = "post_id", on_delete = destroy),
    dependent(PgVoteRepository,    fk = "post_id", on_delete = delete_all),
)]
pub trait PostRepository {}
```

Here `fk` is required, because there is no association leg to infer it from.

### Which one wins

Both are supported, and they drive **the same** generated cascade — the model
form dispatches through a generated `Model::dependents()` table, the repository
form inlines the same calls at compile time. Behavior, ordering, and guarantees
are identical.

When a repository declares `dependent(...)`, **the repository attribute wins for
that repository** and any model-side `dependent` on the same model is inert. That
is deliberate — the repository form exists precisely to override — but a silently
ignored declaration is a trap, so debug builds emit a `tracing::warn!` naming the
model when both are present. The warning rides the single-record delete path
(`delete_by_id`) — an app whose only delete path is `delete_many`
never sees it — and the check is a const-folded `if false` in release builds, so
they pay nothing for it.

Pick one site per model. Reach for the repository form only when the naming
convention cannot reach the child.

### The four actions

| Action       | Effect on matched children                                                              |
|--------------|-----------------------------------------------------------------------------------------|
| `destroy`    | Deletes each child through its own delete path: fires the child's `before_delete` hook (and enqueues its `after_delete_commit` hook when the child repository declares `commit_hooks = true`), honors the child's `soft_delete`, and recurses into the child's own dependents. |
| `delete_all` | One set-based delete of the matched children. No child hooks, **no recursion** — the cheap option for leaf rows. |
| `nullify`    | Sets the child foreign key to `NULL`, leaving the rows. The child's FK column must be nullable (`Option<i64>`). |
| `restrict`   | Refuses the delete with a typed `409 Conflict` if any child still references the parent. |

`destroy` follows the parent's delete kind. A soft-deleting parent soft-deletes
each child that is itself a `#[soft_delete]` model — the live graph stays
consistent and FKs stay valid — and hard-deletes any child that is not, since
there is nothing to soft-delete. A hard-deleting parent hard-deletes every
child, already-soft-deleted rows included, so no row is left dangling behind a
removed parent.

The one child shape that refuses is a **ledgered** soft-delete child under a
hard-deleting parent: hard-deleting it would destroy ledger history, so the
cascade returns a typed conflict instead of removing the row.

### What the cascade guarantees

- **One transaction.** The whole cascade — every level of it — and the parent's
  own delete run inside a single database transaction. Any failure, including a
  `restrict` 409, rolls the entire thing back. The generated delete acquires its
  own connection and opens that transaction itself; it is not `Db::tx` and does
  not nest inside one.
- **A *direct* `restrict` blocks before any hook runs.** Every `restrict`
  declared on the row being deleted is probed up front — before any mutating
  action and before that row's own `before_delete` hook. Deleting a parent whose
  own `restrict` leg is occupied therefore fires no hook at all, on either delete
  path. The probe follows the parent's delete kind: a soft-deleting parent counts
  only *live* children, so an already-soft-deleted child does not block it, while
  a hard-deleting parent counts soft-deleted children too — they would still
  dangle.

  **A deeper `restrict` does not carry that guarantee.** Grandchild and lower
  `restrict` legs are probed inside the cascade that reaches them, not hoisted to
  the top, so work already done can have fired hooks by the time one answers 409:

  - across a parent's **sibling associations** — the mutating pass runs them in
    declaration order, so if the first `destroy` leg's children have hooks and a
    later leg hits a restricting grandchild, those hooks already fired;
  - across **parents in a `delete_many` batch** — Phase 1 probes the batch's
    direct `restrict` legs, but deeper ones wait for each parent's own Phase-2
    cascade.

  Within one association the ordering *is* safe: a `destroy`d child set pre-scans
  every sibling's `restrict` grandchildren before any of their hooks run.

  The transaction always rolls back in full. A hook side effect that is not a
  commit hook does not. So if a `before_delete` hook reaches outside the database,
  put the `restrict` on the leg you are deleting rather than a level below it —
  that is the only placement the up-front probe covers. Deleting parents one at a
  time closes the batch case but not the sibling-association one, since a single
  parent's legs still run in order. Hoisting deeper probes into the up-front pass
  is tracked in [#2427](https://github.com/autumn-foundation/autumn/issues/2427).
- **Both delete paths.** `delete_by_id` and the bulk `delete_many` both run the
  cascade, and on both the children are handled before the parent row goes, so
  bulk-deleting parents neither orphans children nor trips a foreign key. A batch
  is all-or-nothing: a `restrict` 409 anywhere in it rolls back every parent.

  The two paths do **not** interleave hooks identically, and only the database
  effects are guaranteed to match. `delete_by_id` runs the parent's own
  `before_delete` before cascading its children; a hooked `delete_many` cascades
  the whole batch first and runs the parents' `before_delete` hooks after. So a
  parent hook that rejects a bulk delete does so once child hooks have already
  run. As above, the transaction still rolls back in full and a non-commit hook's
  external side effects do not.

  Treat the exact hook interleaving as unspecified beyond what is stated here —
  #2427 tracks pinning it down.
- **Multi-level.** A `destroy`d child runs its **own** dependents before its row
  goes, so `Post → Comment → Reply` cascades end to end. Grandchildren may be
  declared on either site.
- **Cycles terminate.** A `(table, id)` guard tracks the active recursion path,
  so self-referential (`parent_id` on the same table) and mutually-referential
  graphs finish instead of looping.

### Limits and rejected combinations

- `dependent` / `on_delete` on a **`#[belongs_to]`** is a compile error: the
  foreign key lives on that side, so there is no dependent set to cascade into.
  Declare it on the parent's `#[has_many]` / `#[has_one]`.
- `dependent` / `on_delete` on a **`through = <join_table>`** (many-to-many)
  association is a compile error: that association's foreign key names a column
  on the join table, not on the target model, so the cascade would mutate the
  wrong rows.
- An unrecognised action is a compile error naming the four supported spellings —
  never a silently ignored key.
- `delete_all` does not recurse. If the rows it removes have children of their
  own, use `destroy` (or rely on a database-level `ON DELETE`).
- The `delete_many` cascade is **per parent row**: children are cascaded once per
  parent per association rather than in one set-based statement, so a batch of N
  parents costs on the order of N times the per-parent cascade. The batch is
  still one transaction; it is the statement count that scales.
- `retention(...)` and `dependent(...)` cannot be combined on the same
  repository. The sweep mutates rows directly rather than going through the
  cascade-aware delete path, so it would orphan children or ignore a `restrict`
  rule. The repository-attribute combination is a compile error; a model-declared
  cascade on a `retention(...)` repository is rejected at startup with the same
  explanation. Call `delete_many(ids)` from a hand-written `#[scheduled]` sweep
  instead.
- `position(...)` and a repository-attribute `dependent(...)` are also rejected
  together — a move never deletes a row, so the two are independent concerns, but
  the combination is untested. There is no equivalent runtime rejection for a
  model-declared cascade.

See also: [counter caches](counter-cache.md) (how each action moves a parent's
`{child}_count`), [soft delete](soft-delete.md), and
[macro transparency](macro-transparency.md) for the generated shape.

---

## Read Replicas: Automatic Read Routing

When `database.replica_url` is configured, every generated **read-only** method — `find_by_id`, `find_all`, `count`, `exists_by_id`, `paginate`, `cursor_page`, derived `find_by_*` / `count_by_*` / `exists_by_*` queries, and full-text `search` / `search_page` — automatically acquires its connection from the replica pool. Mutating methods (`save`, `update`, `delete_by_id`, the bulk operations, hook-driven writes, `with_lock`) always run on the primary. Provisioning a replica therefore offloads your primary with **zero application code changes**.

When no replica is configured, all methods use the primary — single-pool apps are unaffected.

The routing decision is snapshotted per request from `AppState::read_pool()`, so it honors the `database.replica_fallback` policy: when the replica is unready, reads either fall back to the primary (`replica_fallback = "primary"`) or fail fast with `503 Service Unavailable` (`replica_fallback = "fail_readiness"`) rather than silently serving from the wrong role.

### Opting Out: `primary_reads`

Replica reads can be **stale** by up to your replication lag. For aggregates where a stale read is worse than extra primary load (e.g. account balances, inventory counters), pin the whole repository to the primary:

```rust
#[autumn_web::repository(AccountBalance, primary_reads)]
pub trait AccountBalanceRepository {}
```

All generated reads on this repository use the primary pool, even when a replica is configured. Prefer the per-call escape hatch below when only *some* call sites are read-after-write-sensitive — a repository-wide opt-out gives up replica offloading everywhere.

### Read-Your-Writes: `on_primary()`

After a handler performs a write, an immediate read may land on a replica that has not replayed it yet. The generated `on_primary()` method returns a clone of the repository whose reads are pinned to the primary, so you can read-your-writes without dropping to raw Diesel:

```rust
let created = repo.save(&new_post).await?;
// The replica may not have seen this row yet — read it from the primary.
let fresh = repo.on_primary().find_by_id(created.id).await?;
```

The original `repo` keeps routing reads to the replica; only the pinned clone (and call chains on it) use the primary.

### Transactions

Reads executed inside an explicit transaction (`db.tx(...)` or `repo.with_lock(...)`) run on the transaction's own primary connection — a transaction never splits reads onto a replica.

---

## Performance & Scaling Guidelines

A route's query count can also be pinned at compile time: annotate the handler
with [`#[query_budget(N)]`](query-budgets.md) and the build fails if any
reachable path — including a repository call inside a loop — can exceed `N`.

Caching a repository read is less of a leap of faith than it used to be: every
generated write method publishes the model it mutates, and
[`autumn cache audit`](cache-coherence.md) fails the build when one of them can
leave a `#[cached]` read stale with no `invalidates(...)` covering the pair. The
gate proves the *obligation is discharged in source* — that an invalidation edge
exists and names a real cached read — not that the invalidator runs on every
write path; see that guide's "What this does not prove".


Bulk operations are built for maximum performance, with the following built-in safeguards:

### The Postgres Parameter Ceiling
Postgres supports a maximum of 65,535 parameters per statement. If you try to insert 10,000 rows with 8 columns, that requires 80,000 parameters, which ordinarily crashes.
Autumn automatically calculates the optimal chunk size based on your model's columns and inserts in chunks (e.g. 1000 records at a time) to always remain well below the ceiling while maintaining peak batching throughput (>50x speedups over individual insertions).
