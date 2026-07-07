# Transactions

Use `Db::tx` when a handler must perform **multiple writes atomically**.

If every write in the closure succeeds, the transaction commits. If any step
returns `Err`, the transaction rolls back.

```rust,no_run
use autumn_web::prelude::*;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use scoped_futures::ScopedFutureExt;

async fn create_two_rows(mut db: Db) -> AutumnResult<i64> {
    let id = db
        .tx(|conn| {
            async move {
                let id: i64 = diesel::insert_into(crate::schema::posts::table)
                    .values(crate::schema::posts::title.eq("hello"))
                    .returning(crate::schema::posts::id)
                    .get_result(conn)
                    .await?;

                diesel::insert_into(crate::schema::votes::table)
                    .values((
                        crate::schema::votes::post_id.eq(id),
                        crate::schema::votes::user_id.eq(1_i64),
                        crate::schema::votes::value.eq(1_i16),
                    ))
                    .execute(conn)
                    .await?;

                Ok::<_, AutumnError>(id)
            }
            .scope_boxed()
        })
        .await?;

    Ok(id)
}
```

## `db.tx` vs hooks

- Use repository hooks (`before_create`, `before_update`, `before_delete`) for
  model-local mutation concerns.
- Use `db.tx` when orchestration spans multiple writes and/or multiple tables in
  one route or service operation.

Hooks executed inside `db.tx` participate in the same database transaction.

## Panic and rollback

`Db::tx` delegates to Diesel async transaction handling. Operationally:

- `Ok(_)` commits
- `Err(_)` rolls back
- panics unwind through the transaction boundary and do not commit partial work

## Isolation levels and automatic retry

`db.tx` always runs at Postgres' default **READ COMMITTED**. For
correctness-critical work (ledgers, inventory, uniqueness invariants) you can
request a stronger isolation level — and have transient conflicts retried
automatically — with `db.tx_with(TxOptions::…, |conn| …)`:

```rust,no_run
use autumn_web::prelude::*;
use autumn_web::db::TxOptions;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use scoped_futures::ScopedFutureExt;

async fn transfer(mut db: Db, from: i64, to: i64, cents: i64) -> AutumnResult<()> {
    // SERIALIZABLE + automatic retry: one argument, no hand-rolled retry loop.
    db.tx_with(TxOptions::serializable(), |conn| {
        async move {
            // ... read balances, check invariants, write both rows ...
            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await
}
```

### What each level buys — and costs

| Level | Guards against | Cost |
| --- | --- | --- |
| `read_committed()` (default) | dirty reads | none — each statement sees the latest committed data, so read-then-write races (lost updates, write skew) are possible |
| `repeatable_read()` | non-repeatable & phantom reads | a fixed snapshot per transaction; concurrent writers to your rows abort with `40001` |
| `serializable()` | **all** serialization anomalies, incl. write skew | strongest guarantee; contention surfaces as `40001` and must be retried |

`TxOptions` is a small builder:

```rust
use autumn_web::db::{TxOptions, IsolationLevel};

let opts = TxOptions::serializable()    // Serializable + retry (max_attempts = 5)
    .read_only()                        // SET TRANSACTION READ ONLY
    .max_attempts(10)                   // override the retry budget
    .initial_backoff(std::time::Duration::from_millis(5));
assert_eq!(opts.isolation, IsolationLevel::Serializable);
```

`TxOptions::default()` (and `read_committed()`) is byte-for-byte equivalent to
`db.tx`: READ COMMITTED, one attempt, no retry. `repeatable_read()` and
`serializable()` default to a 5-attempt retry budget because retry is the whole
point at those levels.

### Automatic retry

At REPEATABLE READ and SERIALIZABLE, Postgres rejects transactions that would
break isolation with a **serialization failure** (`40001`); deadlocks surface as
`40P01`. `tx_with` classifies these two SQLSTATEs and re-runs the whole closure,
sleeping a **capped exponential backoff with jitter** between attempts
(`initial_backoff * 2^(n-1)`, capped at `max_backoff`, ±20% jitter). Any other
error, and an exhausted retry budget, propagate as today's `AutumnError` — the
**final** underlying error, never swallowed.

> **The closure must be re-runnable.** Because it can execute more than once, the
> closure must be free of side effects that are not themselves transactional (or
> must be idempotent). Database work is rolled back between attempts, and
> after-commit callbacks from failed attempts are discarded — but any
> non-database side effect in the body (an external API call, a channel send, an
> in-memory mutation) will re-run on every retry. Keep such effects out of the
> closure, or gate them on the final success.

Retries are observable: the transaction runs under a `db.transaction` span
carrying `db.isolation` and the final `db.tx.attempts` count, and each retry
increments the process metric `autumn_tx_retries_total` (exhausted budgets
increment `autumn_tx_retry_exhausted_total`) — both surfaced on the actuator
health endpoint.

### When SERIALIZABLE + retry, `#[lock_version]`, or `with_lock`?

Autumn gives you three tools for concurrent writes; they compose rather than
compete (see the [cloud-native guide](./cloud-native.md) for the locking
attributes):

- **`TxOptions::serializable()` + retry** — the right default when correctness
  depends on an invariant spanning **multiple rows or tables** that a single-row
  version check can't see (write skew: two transactions each read a set and
  write into it). One argument; the database detects the conflict and `tx_with`
  retries.
- **`#[lock_version]` optimistic locking** — best for **low-contention,
  single-row** updates through the generated repository. No stronger isolation
  needed; a stale write returns `RepositoryError::Conflict` (HTTP 409) for the
  client to retry.
- **`with_lock` pessimistic locking** — best for a **hot single row** where you
  want to serialize writers explicitly with `SELECT … FOR UPDATE` and avoid
  wasted retry work.

## Nesting policy

Nested `Db::tx` / `Db::tx_with` calls are **rejected at runtime**:

`Nested Db::tx calls are not supported; use autumn_web::db::savepoint(conn, ..) inside the closure for a same-connection savepoint`

`Db::tx` cannot be re-entered on the same connection — its closure receives
`&mut PooledConnection`, not `&mut Db` — and a second `Db` is a *separate*
connection, which a savepoint cannot model. Keep transaction boundaries explicit
and reach for a savepoint (below) when you need a partial rollback.

### Savepoints via `savepoint`

For a nested, partially-rollbackable unit of work on the **same** connection,
call `autumn_web::db::savepoint(conn, |conn| …)` inside a `tx`/`tx_with` closure.
It issues a Postgres `SAVEPOINT`, releasing it when your closure returns `Ok` and
rolling back to it (`ROLLBACK TO SAVEPOINT`) on `Err` — leaving the surrounding
transaction intact:

```rust,no_run
use autumn_web::prelude::*;
use autumn_web::db::savepoint;
use scoped_futures::ScopedFutureExt;

async fn with_optional_step(mut db: Db) -> AutumnResult<()> {
    db.tx(|conn| {
        async move {
            // ... required write on the outer transaction ...

            // Optional step: if it fails, roll back only this savepoint.
            let _ = savepoint(conn, |conn| {
                async move {
                    // ... best-effort write ...
                    Ok::<_, AutumnError>(())
                }
                .scope_boxed()
            })
            .await;

            // ... more outer-transaction work; commits regardless of the savepoint ...
            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await
}
```

> After-commit callbacks registered inside a savepoint fire when the **outer**
> transaction commits, regardless of whether the savepoint rolled back — the
> callback registry is transaction-scoped, not savepoint-scoped.

---

## `after_commit` — post-commit process-local callbacks

### The dual-write problem

When a handler writes to the database **and** enqueues a job or sends an email,
there are two discrete operations:

- If the side effect runs before the DB commit and the transaction rolls back,
  the side effect fires against data that never existed.
- If the DB commits and the process exits before post-commit work runs, the
  side effect can still be lost.

`after_commit` callbacks solve the first problem only. They are closures
registered inside a `db.tx` block and spawned after the transaction commits
successfully. If the transaction rolls back, the callbacks are discarded.

They are **not a crash-safe delivery mechanism**. The callbacks are
process-local work handed to Tokio after the database commit has already
returned, so a process exit in that window can still lose a Redis enqueue,
external queue publish, email, or other side effect.

For crash-safe delivery, write a durable outbox, Postgres job row, or queue row
inside the same transaction as the domain write, then have a worker drain that
durable record. An `after_commit` callback may still be useful as a wake-up
hint, but the durable row must be the source of truth.

### `register_after_commit`

```rust,no_run
use autumn_web::db::register_after_commit;
use autumn_web::prelude::*;
use scoped_futures::ScopedFutureExt;

async fn create_user(mut db: Db) -> AutumnResult<()> {
    db.tx(|conn| async move {
        // ... INSERT user ...

        // Registers a process-local closure to run AFTER the transaction
        // commits. If the transaction rolls back this closure is dropped.
        register_after_commit(|| async move {
            // Enqueue a job, call an external API, publish an event, etc.
            Ok(())
        })
        .await;

        Ok::<_, AutumnError>(())
    }.scope_boxed())
    .await
}
```

### Jobs — `enqueue_after_commit`

For the common cross-backend case of enqueueing a background job after a
successful write, use the free function `autumn_web::job::enqueue_after_commit`.
It behaves like `JobClient::enqueue` but defers the enqueue until after the
surrounding `db.tx` commits. Outside a transaction it enqueues immediately so it
is safe to call unconditionally.

This is still process-local deferral. If the process exits after commit but
before the callback runs, no job may be recorded. Use it when you need "no job
for rolled-back data"; use a transactional enqueue or durable outbox when the
job handoff itself must survive process loss.

```rust,no_run
use autumn_web::prelude::*;
use scoped_futures::ScopedFutureExt;

async fn publish_post(mut db: Db) -> AutumnResult<()> {
    db.tx(|conn| async move {
        // ... INSERT post ...

        // Enqueued only if the INSERT commits -- no orphaned jobs.
        // Not crash-safe; use enqueue_in_tx for that on Postgres.
        autumn_web::job::enqueue_after_commit("post_publication", &args).await?;

        Ok::<_, AutumnError>(())
    }.scope_boxed())
    .await
}
```

When using the Postgres job backend, prefer `enqueue_in_tx` / `enqueue_on_conn`
for crash-safe job handoff. These APIs write the job row inside the same
database transaction, so the job row and domain row commit or roll back together
atomically. See [Jobs -> Transactional enqueue](jobs.md#transactional-enqueue).

### Mail — auto-deferred `deliver_later`

`Mailer::deliver_later` (and the `deliver_later_*` helpers generated by
`#[mailer]`) automatically detect when they are called inside a `db.tx`
block and defer mail dispatch until the transaction commits. No code change
is required — simply call `deliver_later` inside the closure.

Like any `after_commit` callback, this only prevents mail for rolled-back
writes. It does not make an in-process mail spawn, SMTP send, or external queue
handoff crash-safe unless the configured mail queue records a durable outbox row
or equivalent durable intent.

```rust,no_run
use autumn_web::prelude::*;
use scoped_futures::ScopedFutureExt;

async fn register_user(mut db: Db, mailer: Mailer) -> AutumnResult<()> {
    db.tx(|conn| async move {
        // ... INSERT user ...

        // Automatically deferred until after commit, but not crash-safe by
        // itself.
        AccountMailer.deliver_later_welcome(&mailer, email, username);

        Ok::<_, AutumnError>(())
    }.scope_boxed())
    .await
}
```

To bypass deferral and spawn the mail task immediately regardless of
transaction state, call `deliver_later_eager` instead.

### Repository hooks

The repository macro wires the `after_create_commit`, `after_update_commit`,
and `after_delete_commit` hooks from `MutationHooks` when durable commit hooks
are explicitly enabled on the repository:

```rust,ignore
#[repository(Post, hooks = PostHooks, commit_hooks = true)]
pub trait PostRepository {}
```

Override them to run post-commit side effects without touching the generated
CRUD code:

```rust,no_run
impl MutationHooks for PostHooks {
    async fn after_create_commit(
        &self,
        ctx: &mut RequestContext,
        record: &Post,
    ) -> AutumnResult<()> {
        // Runs after the INSERT commits. Use a durable mail queue/outbox if the
        // notification itself must survive process exit.
        NotificationMailer.deliver_later_new_post(ctx.mailer(), record);
        Ok(())
    }
}
```

When a generated repository mutation runs inside an HTTP request covered by
Autumn idempotency, `MutationContext::idempotency_key` is populated with the
framework-scoped idempotency key. Durable `after_*_commit` queue rows use that
same scoped key to de-duplicate duplicate dispatch rows for a retried request,
and hook implementations can reuse it as a provider idempotency token for
external side effects.

### Observability

A process-level counter tracks failures in after-commit callbacks (for
example, a job broker being unreachable after the DB has already committed):

```rust,no_run
autumn_web::db::AFTER_COMMIT_FAILURES_TOTAL.load(std::sync::atomic::Ordering::Relaxed)
```

Scrape this counter in your metrics handler or dashboards. A non-zero value
means at least one committed transaction's side effect was not delivered and
may need manual recovery.

### When to use which approach

| Scenario | Recommended API |
|---|---|
| Job + DB write on any backend, avoiding rolled-back data | `enqueue_after_commit` inside `db.tx` |
| Crash-safe job + DB write on Postgres | `enqueue_in_tx` / `enqueue_on_conn` inside `db.tx` |
| Email triggered by a DB write, avoiding rolled-back data | `deliver_later` inside `db.tx` (auto-deferred) |
| Crash-safe email triggered by a DB write | Insert a durable outbox row in the transaction; use a mail queue/worker to drain it |
| Repository create/update/delete side effect | `after_create_commit` / `after_update_commit` / `after_delete_commit` hook with `commit_hooks = true` |
| Custom side effect on commit | `register_after_commit` inside `db.tx` |

## Bulk Repository Operations & Transactions

All generated bulk methods (`save_many`, `update_many`, `delete_many`, `upsert_many`) fully integrate with Autumn's transaction boundaries:

- **Atomic Execution**: On repositories with hooks configured, the entire batch query and hook execution are wrapped in an atomic database transaction. If any individual record hook fails or if the database returns an error, the entire operation is automatically rolled back.
- **Participation in `db.tx`**: If a bulk operation is called inside a `db.tx` block, it automatically participates in that outer transaction. No new nested transaction is started, conforming to Autumn's nesting policy.
- **Durable Commit Hooks**: If commit hooks are enabled (`commit_hooks = true`), post-commit hooks like `after_create_commit` will be staged during bulk writes and executed sequentially only when the surrounding database transaction successfully commits.

## Transactions and Read Replicas

When `database.replica_url` is configured, generated repository read methods normally route to the replica pool (see the [repositories guide](repositories.md#read-replicas-automatic-read-routing)). Transactions are the exception: **everything inside a transaction stays on the single primary connection that owns it**.

- `db.tx(|conn| ...)` hands your closure the transaction's primary connection; every query you run on `conn` — reads included — executes on the primary. There is no split-brain where a transaction writes to the primary but reads stale data from a replica.
- `repo.with_lock(id, |record, conn| ...)` performs its `SELECT ... FOR UPDATE` and runs your closure on a primary transaction connection, since locking reads are only meaningful on the writer.
- The internal transactions opened by generated write methods (`save`, `update`, bulk operations, hook lifecycles) acquire from the primary pool, so hook-driven reads-during-write also see the primary.

For read-your-writes *outside* a transaction — e.g. a handler that saves and then re-fetches — use the repository's `on_primary()` escape hatch instead of opening a transaction just to pin the connection.
