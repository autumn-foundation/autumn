# Hooks & Transactions

This guide covers every layer of Autumn's mutation pipeline: database transactions,
repository lifecycle hooks, post-commit callbacks, and bulk operations. Each layer
has a distinct scope and different error semantics. Understanding how they compose
is the key to writing correct, side-effect-safe code.

---

## Mental model

Autumn gives you three concentric boundaries around a mutation:

```
┌─────────────────── db.tx ───────────────────────────────────┐
│                                                              │
│  ┌────────────── repository mutation ───────────────────┐   │
│  │                                                       │   │
│  │  before_* hook → SQL → after_* hook                  │   │
│  │                                                       │   │
│  └───────────────────────────────────────────────────────┘   │
│                                                              │
│  register_after_commit / enqueue_after_commit                │
│  deliver_later (auto-deferred)                               │
│                                                              │
└──────────────────────────────────────────────────────────────┘
           │
           ▼ (after Postgres confirms COMMIT)
  after_*_commit hook  (durable, crash-safe — requires commit_hooks = true)
```

- **`db.tx`** — wraps any number of SQL statements (or repository calls) in a
  single Postgres transaction.
- **Repository hooks** (`before_*`, `after_*`) — run inside whichever transaction
  is active when the repository method is called.
- **`after_commit` callbacks** — registered inside a transaction, run after the
  transaction durably commits. Process-local; not crash-safe by themselves.
- **`after_*_commit` hooks** — durable, Postgres-backed, survive process crashes.
  Opt-in per repository.

---

## `db.tx` — explicit atomic boundaries

Use `db.tx` when a handler must write to multiple tables and all writes must
succeed or fail together.

```rust,no_run
use autumn_web::prelude::*;
use scoped_futures::ScopedFutureExt;

async fn accept_order(mut db: Db, order_id: i64) -> AutumnResult<()> {
    db.tx(|conn| {
        async move {
            diesel::update(orders::table.find(order_id))
                .set(orders::status.eq("accepted"))
                .execute(conn)
                .await?;

            diesel::insert_into(audit_events::table)
                .values(audit_events::order_id.eq(order_id))
                .execute(conn)
                .await?;

            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await
}
```

The closure receives a raw `&mut PooledConnection`. Return `Ok(_)` to commit,
return `Err(_)` to roll back. Panics inside the closure also roll back without
committing partial work.

### Nesting policy

Nested `db.tx` calls are **rejected at runtime**:

```
Nested Db::tx calls are not supported
```

This is intentional. Repository methods always acquire their own connection
from the pool and manage their own internal transaction — they do not share the
`db.tx` connection. Calling a repository method inside `db.tx` will not trigger
the nesting error, but the two operations are on separate connections and
separate transactions. If subsequent SQL in the `db.tx` closure fails and rolls
back, writes already committed by the repository call are **not rolled back**.

---

## Repository lifecycle hooks

Hooks give you a place to run model-scoped logic — validation, enrichment,
derived-field updates, side effects — without scattering that logic across
handlers.

### Opting in

```rust,no_run
use autumn_web::hooks::{MutationContext, MutationHooks, UpdateDraft};

#[derive(Clone, Default)]
pub struct ArticleHooks;

impl MutationHooks for ArticleHooks {
    type Model = Article;
    type NewModel = NewArticle;
    type UpdateModel = UpdateArticle;

    // override only the methods you need
}

#[repository(Article, hooks = ArticleHooks)]
pub trait ArticleRepository {}
```

The hooks struct must implement `Default` and `Clone`. Every method has a
default no-op implementation, so you only pay for what you override.

---

### `MutationContext`

Every hook receives a `&mut MutationContext`. It carries:

| Field | Type | Description |
|---|---|---|
| `op` | `MutationOp` | `Create`, `Update`, or `Delete` |
| `actor` | `Option<String>` | User ID or service name |
| `request_id` | `Option<String>` | UUID v4, auto-generated per mutation |
| `now` | `DateTime<Utc>` | Timestamp, auto-populated |
| `invalidate_keys` | `Vec<String>` | User-managed list of cache keys; Autumn collects them but does not act on them automatically |
| `idempotency_key` | `Option<String>` | Scoped HTTP idempotency key |

Useful methods:

```rust,no_run
// Push a key into ctx.invalidate_keys.
// Autumn does not consume this list automatically — read it yourself in an
// after_*_commit hook or cache middleware to perform the actual invalidation.
ctx.invalidate(format!("article:{}", record.id));

// Set an explicit idempotency key for durable side-effect deduplication
ctx.set_idempotency_key("my-scoped-key");
```

---

### `before_create`

Called before the `INSERT`, inside the transaction. Receives a mutable
reference to the new-record struct so you can enrich or normalize it before
it is written.

The model's own `#[normalize]` and `#[validate]` rules have already run by
then (#2586), so a field this hook *fills in* — the derived slug below — is
judged before it exists. Do not put a `#[validate]` rule on a hook-populated
column: it would reject every insert. Rules belong on what the caller supplies.

```rust,no_run
async fn before_create(
    &self,
    ctx: &mut MutationContext,
    new: &mut NewArticle,
) -> AutumnResult<()> {
    // Derive the slug from the title before the record is stored
    new.slug = slugify(&new.title);

    // Reject: returning Err prevents the INSERT and rolls back
    if new.title.trim().is_empty() {
        return Err(AutumnError::validation("title", "can't be blank"));
    }

    Ok(())
}
```

Returning `Err` prevents the mutation entirely. No SQL runs.

---

### `before_update`

Called before the `UPDATE`, inside the transaction. Receives an `UpdateDraft<T>`
that holds both the original (`before`) and proposed (`after`) model state.

```rust,no_run
async fn before_update(
    &self,
    ctx: &mut MutationContext,
    draft: &mut UpdateDraft<Article>,
) -> AutumnResult<()> {
    // Re-derive the slug only when the title actually changed
    if draft.after.title != draft.before.title {
        draft.after.slug = slugify(&draft.after.title);
    }

    // Stamp approved_at when status transitions to Approved
    if draft.after.status == Status::Approved && draft.before.status != Status::Approved {
        draft.after.approved_at = Some(ctx.now);
    }

    Ok(())
}
```

#### `UpdateDraft<T>`

| Method | Description |
|---|---|
| `draft.before()` | Reference to the original model |
| `draft.after()` | Reference to the proposed model |
| `draft.after_mut()` | Mutable reference to the proposed model |
| `draft.into_after()` | Consume the draft, return the proposed model |

#### `DraftField<T>` — per-field accessors

The `#[model]` macro generates per-field accessors that return a `DraftField`
borrowing from the draft. This lets you inspect and override individual fields
without juggling split borrows manually:

```rust,no_run
// Reading
draft.status().before()             // &Status  — value before mutation
draft.status().after()              // &Status  — proposed value
draft.status().changed()            // bool
draft.status().changed_to(&Status::Published)  // bool

// For Option<T> fields
draft.published_at().was_set()      // None → Some transition
draft.published_at().was_cleared()  // Some → None transition

// Overriding
draft.slug().set(slugify(&draft.after.title));
```

---

### `before_delete`

Called before the `DELETE`, inside the transaction. Receives a read-only
reference to the record about to be deleted.

```rust,no_run
async fn before_delete(
    &self,
    ctx: &mut MutationContext,
    record: &Article,
) -> AutumnResult<()> {
    if record.status == Status::Published {
        return Err(AutumnError::validation("status", "published articles cannot be deleted"));
    }
    Ok(())
}
```

---

### `after_create` and `after_update`

Called **after the transaction commits and the connection is released**, but
before the repository method returns to the caller. The data is already
durably written. Returning `Err` propagates back to the caller as an error
but **does not rollback the committed record**.

Because these hooks run after the data is committed, they are not suitable for
validation that should prevent a write. Use `before_*` for that. Their main
uses are:

- Synchronous in-process operations against committed data (e.g., updating an
  in-memory cache that must be consistent with the just-written row)
- Collecting cache keys via `ctx.invalidate()` so a post-commit step can act on them
- Triggering in-process event buses or notification channels

```rust,no_run
async fn after_create(
    &self,
    ctx: &mut MutationContext,
    record: &Article,
) -> AutumnResult<()> {
    // Collect cache keys to invalidate once the record is committed.
    ctx.invalidate(format!("articles:author:{}", record.author_id));
    ctx.invalidate("articles:recent");
    Ok(())
}
```

---

### Hook error semantics — summary

| Hook | Error behavior |
|---|---|
| `before_create` | Prevents INSERT; transaction rolls back |
| `before_update` | Prevents UPDATE; transaction rolls back |
| `before_delete` | Prevents DELETE; transaction rolls back |
| `after_create` | Called after commit; error propagates to caller, but data is **already committed** |
| `after_update` | Called after commit; error propagates to caller, but data is **already committed** |
| `after_create_commit` | Error is logged and counted; mutation is already committed |
| `after_update_commit` | Error is logged and counted; mutation is already committed |
| `after_delete_commit` | Error is logged and counted; mutation is already committed |

---

### `Patch<T>` — partial-update payloads

`Patch<T>` is a tri-state enum used in update changesets (the `UpdateModel`).
It distinguishes three cases that a plain `Option<T>` cannot:

```rust,no_run
pub enum Patch<T> {
    Unchanged,  // field was absent from the request — do nothing
    Set(T),     // field was explicitly set to a value
    Clear,      // field was explicitly set to null
}
```

When deserializing a JSON PATCH body, an absent field deserializes as
`Unchanged`, a `null` value deserializes as `Clear`, and any other value
deserializes as `Set(v)`. Mark fields with `#[serde(default)]` to wire
this up automatically.

```rust,no_run
// Constructing an update request — Patch<T> is the type of UpdateModel fields:
let update = UpdateArticle {
    status:       Patch::Set(Status::Published),  // explicitly set
    published_at: Patch::Set(Utc::now()),         // explicitly set
    title:        Patch::Unchanged,               // leave as-is (default)
    summary:      Patch::Clear,                   // explicitly clear to NULL
    ..Default::default()
};

// Inside before_update, draft.before and draft.after are both fully
// materialized Model values (the Patch has already been applied).
// Use DraftField accessors to inspect transitions — not Patch matching:
if draft.published_at().was_set() {
    // None → Some: article is being published for the first time
}
if draft.published_at().was_cleared() {
    // Some → None: published_at is being explicitly cleared
}
```

---

## `after_commit` — post-commit process-local callbacks

Some side effects must not fire against rolled-back data — job enqueues,
outbound emails, external API calls. `after_commit` callbacks are registered
inside a `db.tx` block and run only if the transaction commits successfully.
If the transaction rolls back, the callbacks are discarded.

### `register_after_commit`

```rust,no_run
use autumn_web::db::register_after_commit;

async fn publish_article(mut db: Db) -> AutumnResult<()> {
    db.tx(|conn| async move {
        // ... UPDATE article status ...

        register_after_commit(|| async move {
            // Runs only if the UPDATE commits.
            // Failures here are logged but do not rollback the mutation.
            cdn::purge_cache("articles").await?;
            Ok(())
        })
        .await;

        Ok::<_, AutumnError>(())
    }.scope_boxed())
    .await
}
```

### Jobs — `enqueue_after_commit`

```rust,no_run
use autumn_web::job::enqueue_after_commit;

async fn create_user(mut db: Db) -> AutumnResult<()> {
    db.tx(|conn| async move {
        // ... INSERT user ...

        // Enqueued only if the INSERT commits. No orphaned jobs.
        enqueue_after_commit("send_welcome_email", &args).await?;

        Ok::<_, AutumnError>(())
    }.scope_boxed())
    .await
}
```

For crash-safe job enqueue on Postgres, use `enqueue_in_tx` / `enqueue_on_conn`
instead — these write the job row inside the same transaction so the job and
the domain data commit atomically. See [Jobs → Transactional enqueue](jobs.md#transactional-enqueue).

### Mail — `deliver_later` (auto-deferred)

`Mailer::deliver_later` automatically detects a surrounding `db.tx` and defers
dispatch until after commit. No code change needed.

```rust,no_run
async fn register_user(mut db: Db, mailer: Mailer) -> AutumnResult<()> {
    db.tx(|conn| async move {
        // ... INSERT user ...

        // Automatically deferred; no mail for rolled-back registrations.
        AccountMailer.deliver_later_welcome(&mailer, email, username);

        Ok::<_, AutumnError>(())
    }.scope_boxed())
    .await
}
```

To bypass deferral and dispatch immediately regardless of transaction state,
call `deliver_later_eager` instead.

### The crash-safety gap

`after_commit` callbacks are process-local. The sequence is:

```
Postgres confirms COMMIT → Tokio spawns callback → callback runs
```

If the process exits between the first and second step, the callback is lost.
For side effects that must survive process crashes, write a durable record
(outbox row, Postgres job row) inside the transaction itself, then have a
worker drain it. The callback can still be a useful wake-up hint, but the
durable record must be the source of truth.

### Observability

A process-level counter tracks `after_commit` failures (job broker down, SMTP
unreachable, etc.):

```rust,no_run
autumn_web::db::AFTER_COMMIT_FAILURES_TOTAL.load(std::sync::atomic::Ordering::Relaxed)
```

A non-zero value means at least one committed transaction's side effect was not
delivered. Scrape this counter in your metrics handler or health dashboard.

---

## `after_*_commit` hooks — durable, crash-safe

The `after_create_commit`, `after_update_commit`, and `after_delete_commit`
hook methods fire after the transaction has durably committed to Postgres, and
unlike the process-local `after_commit` callbacks above, they survive process
crashes.

### Opting in

Enable them per repository with `commit_hooks = true`:

```rust,no_run
#[repository(Article, hooks = ArticleHooks, commit_hooks = true)]
pub trait ArticleRepository {}
```

This tells the generated code to write an intent row into Autumn's
framework-owned `autumn_repository_commit_hooks` table in the same transaction
as the mutation. Workers claim and execute those rows using Postgres row locks,
so a process exit before execution is recovered by retry rather than silent
loss.

### Implementing the hooks

```rust,no_run
impl MutationHooks for ArticleHooks {
    // ...

    async fn after_create_commit(
        &self,
        ctx: &mut MutationContext,
        record: &Article,
    ) -> AutumnResult<()> {
        // Runs after the INSERT has durably committed.
        // Errors are logged and retried — they do NOT rollback the mutation.
        NotificationMailer
            .deliver_later_new_article(ctx.mailer(), record)
            .await
    }

    async fn after_update_commit(
        &self,
        ctx: &mut MutationContext,
        record: &Article,
    ) -> AutumnResult<()> {
        ctx.invalidate(format!("article:{}", record.id));
        cdn::purge_article(record.id).await
    }

    async fn after_delete_commit(
        &self,
        ctx: &mut MutationContext,
        record: &Article,
    ) -> AutumnResult<()> {
        search_index::delete(record.id).await
    }
}
```

### How the durable queue works

| State | Meaning |
|---|---|
| `enqueued` | Row written in same transaction as mutation; ready to claim |
| `running` | Claimed by a worker; heartbeat renewed every 15 s |
| `completed` | Successfully executed |
| `failed` | Exhausted retries (default: 5, with exponential backoff) |

If a worker crashes mid-execution, the stale claim (> 60 s without heartbeat)
is recovered by another worker.

Execution is **at-least-once**. A transient failure causes the hook to be
retried (up to 5 attempts, exponential backoff), and stale-claim recovery can
also re-execute a hook that appeared to start but never completed. Idempotency
keys deduplicate rows produced by a _retried HTTP request_ (the same logical
mutation submitted twice), but a single mutation's commit hook can still run
more than once due to retries. Design `after_*_commit` implementations to be
idempotent.

### Error semantics

Failures in `after_*_commit` are **not** propagated back to the caller and do
**not** rollback the committed mutation. They are logged and counted. If the
failure is persistent, the row reaches `failed` state and requires manual
recovery or investigation. Design these hooks to be idempotent.

---

## Bulk operations

All bulk methods participate in the same transaction model as single-record
mutations.

### Available methods

```rust,no_run
// Insert a batch of records
repo.save_many(&[NewArticle { ... }, NewArticle { ... }]).await?;

// Insert, skipping invalid rows rather than aborting the whole batch
let (saved, errors) = repo.save_many_skip_invalid(&rows).await?;

// Apply the same changeset to multiple records by ID
// UpdateModel fields are Patch<T>, not Option<T>
repo.update_many(&[1, 2, 3], &UpdateArticle {
    status: Patch::Set(Status::Archived),
    ..Default::default()
}).await?;

// Delete multiple records by ID
repo.delete_many(&[4, 5, 6]).await?;

// Insert-or-update by primary key — only available on hook-free repositories
repo.upsert_many(&[article_a, article_b]).await?;
```

### Transaction behavior

**Without hooks:** bulk methods still wrap their chunked SQL in an explicit
transaction for all-or-nothing atomicity — `begin`, one or more chunk
INSERT/UPDATE/DELETE statements, `commit`. If any chunk fails the whole batch
rolls back. There is no extra round-trip beyond the chunked SQL itself.

**With hooks:** `before_*` hooks and all SQL run inside the transaction and can
cause a rollback on error. `after_*` hooks run **after the transaction commits**
(same as single-record mutations) — errors in `after_*` propagate to the caller
but the batch rows are already committed and cannot be rolled back from there.

```
save_many with hooks:
  (own connection acquired from pool)
  BEGIN
    before_create(record_0)
    before_create(record_1)
    ...
    INSERT INTO ... VALUES (...), (...), ...   ← chunked batch inserts
    stage after_create_commit rows             ← if commit_hooks = true
  COMMIT
  (connection released)
  after_create(record_0)                       ← post-commit; error ≠ rollback
  after_create(record_1)
  ...
  → after_create_commit dispatched to workers
```

`update_many` with hooks issues **one `UPDATE` per record** (not a single bulk
statement) inside the transaction, plus a `SELECT ... FOR UPDATE` beforehand
to load current state for `before_update` hooks. For N records that is
1 SELECT + N UPDATEs, all inside a single transaction. `delete_many` follows
the same pattern: 1 SELECT + N DELETEs.

### Isolation from `db.tx`

Repository methods (bulk or single-record) always acquire their own connection
from the pool. They do **not** share the connection inside a `db.tx` block, and
their internal transaction commits independently.

```rust,no_run
db.tx(|conn| async move {
    // repo.save_many acquires a SEPARATE connection from the pool and
    // commits its own transaction. If the diesel::update below fails and
    // db.tx rolls back, the save_many writes are NOT rolled back.
    repo.save_many(&new_articles).await?;

    diesel::update(summary::table)
        .set(summary::count.eq(summary::count + new_articles.len() as i64))
        .execute(conn)
        .await?;

    Ok::<_, AutumnError>(())
}.scope_boxed())
.await?;
```

For operations that must be atomic across multiple tables, write all SQL
directly inside `db.tx` using the `conn` from the closure — do not mix
repository calls with other SQL if atomicity is required.

### `save_many_skip_invalid`

When bulk-importing dirty external data you may want to save the valid rows
and surface errors for the invalid ones rather than aborting the entire batch.

```rust,no_run
let rows: Vec<NewArticle> = parse_csv(upload)?;
let (saved, errors) = repo.save_many_skip_invalid(&rows).await?;

for (index, error) in &errors {
    tracing::warn!(row = index, error = %error, "skipped invalid row");
}
```

Rows are normalized, then checked against the model's `#[validate]` rules, then
passed to `before_create`; a failure at any of those steps is filtered out and
reported against the caller's own index. If the resulting batch insert fails due
to a database constraint (e.g. a unique violation), Autumn falls back to
row-by-row insertion for that chunk so that individual constraint failures are
isolated rather than aborting all remaining valid rows.

### `upsert_many` and hooks — a compile-time guard

`upsert_many` uses `INSERT ... ON CONFLICT (id) DO UPDATE`. Whether a
given row will insert or update is determined by Postgres at execution time —
not before the query runs. That makes it impossible to call the correct hook
(`before_create` vs `before_update`) before sending the statement.

To prevent silently bypassing hooks, calling `upsert_many` on a repository
that has hooks configured is **rejected at compile time**. Use `save_many` or
`update_many` explicitly when hooks are in play.

### Parameter ceiling and chunking

Postgres supports at most 65,535 bound parameters per statement. Autumn
calculates the maximum chunk size for your model's column count and splits
large batches automatically. You never need to chunk manually.

| Model columns | Max records per chunk |
|---|---|
| 5 | 1,000 (capped) |
| 10 | 1,000 (capped) |
| 50 | 1,000 (capped) |

Each chunk is inserted as a separate statement within the same transaction, so
atomicity is preserved across chunks.

---

## Decision guide

| What you need | Use |
|---|---|
| Multiple tables written atomically in a handler | `db.tx` |
| Enforce a rule on a field the caller supplies, on every insert | `#[validate]` on the `#[model]` — it runs on every generated insert path, before this hook |
| Enrich a record, or check something the model cannot see (another table, the request context), before every insert | `before_create` |
| Derive a field from another on every update (e.g. slug from title) | `before_update` |
| Prevent deletion based on model state | `before_delete` |
| Write multiple tables atomically | `db.tx` with raw Diesel — repository methods acquire their own connection and cannot share a `db.tx` transaction |
| Enqueue a job only if the DB write commits (no crash safety needed) | `enqueue_after_commit` inside `db.tx` |
| Crash-safe job enqueue on Postgres | `enqueue_in_tx` / `enqueue_on_conn` |
| Send email only if the write commits | `deliver_later` inside `db.tx` (auto-deferred) |
| Post-commit side effect scoped to a single repository model | `after_*_commit` with `commit_hooks = true` |
| Crash-safe post-commit side effect | `after_*_commit` with `commit_hooks = true` |
| Custom post-commit side effect in a handler | `register_after_commit` inside `db.tx` |

---

## Footguns

### Nested `db.tx` is rejected at runtime

There is no savepoint support. If code deeper in the call stack tries to open
a second `db.tx` while one is already active, it returns `Err` immediately:

```
Nested Db::tx calls are not supported
```

This is a runtime `Err`, not a panic — the outer transaction is still live and
will roll back normally when the error propagates. Repository methods each
acquire their own pool connection, so calling one inside `db.tx` does not add
it to the outer transaction. Nesting `db.tx` directly is the only thing
rejected.

### `after_commit` callbacks are not crash-safe

Registering a callback with `register_after_commit` (or calling
`enqueue_after_commit` or `deliver_later`) does not make the side effect
durable. If the process exits in the window between Postgres confirming the
commit and Tokio executing the callback, the side effect is lost with no
record of it. Use `after_*_commit` hooks with `commit_hooks = true`, or write
a durable outbox row in the transaction, if you need guaranteed delivery.

### `after_*` hooks run after the transaction — validation belongs in `before_*`

`after_create` and `after_update` run **after the transaction commits**. The
data is already in the database when these hooks fire. Returning `Err`
propagates back to the caller as an error, but the record has already been
written. Do not use `after_*` hooks for "this must not persist" validation —
that logic belongs in `before_create` or `before_update`, which run before
any SQL executes and can reject the mutation entirely.

### `before_update` pays a `SELECT FOR UPDATE` cost

Both single-record `.update()` and bulk `update_many()` perform a
`SELECT ... FOR UPDATE` before running `before_update` hooks, because the hook
needs the existing record state. This is an extra round-trip to the database.

On repositories with **no update hooks**, the pre-update `SELECT` is skipped
_unless_ the `UpdateModel` carries an optimistic lock-version field — in that
case the generated code still issues `SELECT ... FOR UPDATE` to verify the
version before writing, regardless of whether hooks are configured. Do not add
`before_update` hooks purely to document intent; every hook invocation pays for
the `SELECT`.

### `update_many` applies one changeset to every record

```rust,no_run
// Sets status = "archived" on ALL three records
// UpdateModel fields are Patch<T> — use Patch::Set, not Some(...)
repo.update_many(&[1, 2, 3], &UpdateArticle {
    status: Patch::Set(Status::Archived),
    ..Default::default()
}).await?;
```

There is no per-record changeset variant. If you need different changes per
record and true atomicity, write the updates as raw Diesel SQL directly inside
a `db.tx` closure using the `conn` parameter — repository methods acquire their
own pool connections and cannot participate in an outer `db.tx` transaction.

### `upsert_many` silently bypasses hooks — and is blocked at compile time

There is no runtime fallback that would call the "right" hook for upsert. The
compile-time rejection is there to prevent a subtle bug where hooks appear to
be configured but are never called. If you see the compile error, you need to
decide: does this data need `save_many` (inserts with `before_create`) or
`update_many` (updates with `before_update`)?

---

## Worked example — all layers together

The scenario: a content platform where publishing articles must normalize slugs,
stamp `published_at` only on the first draft→published transition, collect cache
keys to invalidate, dispatch a durable post-commit webhook, and enqueue a
low-priority summary job.

```rust,no_run
// hooks.rs
use autumn_web::hooks::{MutationContext, MutationHooks, Patch, UpdateDraft};

#[derive(Clone, Default)]
pub struct ArticleHooks;

impl MutationHooks for ArticleHooks {
    type Model = Article;
    type NewModel = NewArticle;
    type UpdateModel = UpdateArticle;

    // Normalize on create: derive slug, set default status, reject blank title.
    async fn before_create(
        &self,
        _ctx: &mut MutationContext,
        new: &mut NewArticle,
    ) -> AutumnResult<()> {
        if new.title.trim().is_empty() {
            return Err(AutumnError::validation("title", "can't be blank"));
        }
        new.slug = slugify(&new.title);
        new.status = Status::Draft;
        Ok(())
    }

    // Derive slug when title changes; stamp published_at on the first
    // draft → published transition only.
    async fn before_update(
        &self,
        ctx: &mut MutationContext,
        draft: &mut UpdateDraft<Article>,
    ) -> AutumnResult<()> {
        if draft.after.title != draft.before.title {
            draft.after.slug = slugify(&draft.after.title);
        }
        // Gate on both the status transition AND published_at being unset.
        // Without the second guard, republishing a previously-published article
        // (draft → published → draft → published) would silently overwrite the
        // original publish timestamp.
        if draft.status().changed_to(&Status::Published) && draft.before.published_at.is_none() {
            draft.after.published_at = Some(ctx.now);
        }
        Ok(())
    }

    // after_update runs after the transaction commits. Returning Err here
    // propagates to the caller but does NOT rollback the committed record.
    // Use it for in-process operations against committed data.
    async fn after_update(
        &self,
        ctx: &mut MutationContext,
        record: &Article,
    ) -> AutumnResult<()> {
        ctx.invalidate(format!("article:{}", record.id));
        ctx.invalidate(format!("articles:author:{}", record.author_id));
        Ok(())
    }

    // Durable post-commit webhook — at-least-once execution, so the receiver
    // must be idempotent. This fires for every update to a published article,
    // not just the first publish; receivers should deduplicate by record ID +
    // updated_at or a stable event key.
    async fn after_update_commit(
        &self,
        ctx: &mut MutationContext,
        record: &Article,
    ) -> AutumnResult<()> {
        if record.status == Status::Published {
            webhooks::dispatch("article.published", record).await?;
        }
        // Act on cache keys collected in after_update.
        for key in &ctx.invalidate_keys {
            cache::invalidate(key).await?;
        }
        Ok(())
    }
}

// repositories.rs
#[repository(Article, hooks = ArticleHooks, commit_hooks = true)]
pub trait ArticleRepository {}

// handlers.rs — publish a batch of articles.
// repo.update_many manages its own connection and transaction; it does not
// share the connection with any surrounding db.tx. To write other tables
// atomically with article updates, write raw SQL in a separate db.tx instead.
async fn bulk_publish(repo: ArticleRepo, ids: Vec<i64>) -> AutumnResult<()> {
    // update_many: before_update per record → N individual UPDATEs → COMMIT
    // after_update per record → after_update_commit staged → workers dispatch
    // UpdateModel fields are Patch<T> — use Patch::Set, not Some(...).
    repo.update_many(&ids, &UpdateArticle {
        status: Patch::Set(Status::Published),
        ..Default::default()
    }).await?;

    // Called outside db.tx → enqueues immediately.
    // Use enqueue_in_tx inside a db.tx for crash-safe handoff.
    enqueue_after_commit("bulk_publish_summary", &BulkPublishArgs { ids }).await?;

    Ok(())
}
```

Execution order when `bulk_publish` is called with `ids = [1, 2, 3]`:

```
(repo acquires own connection from pool)
BEGIN
  SELECT id, ... FROM articles WHERE id IN (1,2,3) FOR UPDATE
  before_update(article_1)  → derive slug if title changed, stamp published_at
  before_update(article_2)  → ...
  before_update(article_3)  → ...
  UPDATE articles SET ... WHERE id = 1     ← one UPDATE per record
  UPDATE articles SET ... WHERE id = 2
  UPDATE articles SET ... WHERE id = 3
  INSERT INTO autumn_repository_commit_hooks ... (3 rows)
COMMIT
(connection released)
after_update(article_1)      → ctx.invalidate("article:1", ...)  ← post-commit, pre-return
after_update(article_2)      → ...
after_update(article_3)      → ...
enqueue_after_commit enqueues "bulk_publish_summary" immediately (outside tx)
Workers claim: after_update_commit(article_1) → webhooks::dispatch(...), cache::invalidate(...)
               after_update_commit(article_2) → ...
               after_update_commit(article_3) → ...
```

If the transaction fails at any point before `COMMIT`, it rolls back — no
partial publishes, no stale commit-hook rows. The `after_update` calls and
`enqueue_after_commit` only run after the commit succeeds. The
`after_update_commit` rows were never committed, so workers never see them.
