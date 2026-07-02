# Background Jobs (`#[job]`)

Autumn provides first-class ad-hoc background jobs for request-triggered async work.

## Define a job

```rust,ignore
use autumn_web::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WelcomeEmailArgs {
    pub user_id: i64,
}

#[job(name = "send_welcome_email", max_attempts = 6, backoff_ms = 500)]
async fn send_welcome_email(state: AppState, args: WelcomeEmailArgs) -> AutumnResult<()> {
    // perform async side effect
    Ok(())
}
```

## Register jobs

```rust,ignore
autumn_web::app()
    .routes(routes![signup])
    .jobs(jobs![send_welcome_email])
    .run()
    .await;
```

## Enqueue from handlers

```rust,ignore
SendWelcomeEmailJob::enqueue(WelcomeEmailArgs { user_id: 42 }).await?;
```

## Delayed and scheduled jobs

Sometimes you want a job to run **once, at a future time** — "email a signup
reminder in 24h", "expire this cart in 30 minutes", "publish at 9am", "retry
this external call in 5 minutes". Use `enqueue_in` (relative delay) or
`enqueue_at` (absolute instant) instead of `enqueue`:

```rust,ignore
use std::time::Duration;

// Run once, 24 hours from now.
SendReminderJob::enqueue_in(ReminderArgs { user_id: 42 }, Duration::from_secs(24 * 60 * 60)).await?;

// Run once, at an absolute UTC instant.
let when = chrono::Utc::now() + chrono::TimeDelta::hours(2);
PublishPostJob::enqueue_at(PublishArgs { post_id: 7 }, when).await?;
```

The same free functions exist on the `job` module
(`autumn_web::job::enqueue_in(name, payload, delay)` /
`enqueue_at(name, payload, when)`), mirroring `enqueue`.

A delayed job is recorded immediately but is **not delivered to a worker until
its due time passes**. Once due, it runs through the normal path — the same
`max_attempts` / `initial_backoff_ms` retry/backoff and dead-letter semantics
apply unchanged. An `enqueue_at` time in the past runs immediately.

### Transactional delayed enqueue

Delayed enqueue composes with the transactional variants, so a job is invisible
to workers until **both** the row commits **and** the due time passes:

```rust,ignore
use scoped_futures::ScopedFutureExt;

// Crash-safe on Postgres: the future run time is written inside your tx.
db.tx(move |conn| async move {
    let cart = carts::create(new_cart, conn).await?;
    autumn_web::job::enqueue_in_on_conn(
        "expire_cart",
        ExpireArgs { cart_id: cart.id },
        Duration::from_secs(30 * 60),
        conn,
    ).await?;
    Ok(cart)
}.scope_boxed()).await?;

// Process-local after-commit defer (not crash-safe), absolute or relative:
autumn_web::job::enqueue_in_after_commit("send_reminder", args, Duration::from_secs(3600)).await?;
autumn_web::job::enqueue_at_after_commit("publish_post", args, when).await?;
```

### Durability

| Backend    | Pending delay survives restart? | How                                   |
|------------|---------------------------------|---------------------------------------|
| `postgres` | **Yes** (crash-safe)            | future `run_at` column; claim query skips it until due |
| `redis`    | **Yes** (crash-safe)            | `:delayed` ZSET scored by due-time; promoted to the queue when due |
| `local`    | **No** (local-safe only)        | in-process timer; a pending delay is **lost on restart**, consistent with other in-process caveats |

### Pick the right tool

| Need                                            | Use                          |
|-------------------------------------------------|------------------------------|
| **Recurring** work on a cron / fixed interval   | `#[scheduled]`               |
| **One-shot** "run once, later" timer            | delayed `#[job]` (`enqueue_in` / `enqueue_at`) |
| **Durable multi-step** orchestration, long-horizon timers, history | Autumn Harvest |

`#[scheduled]` is for repeating tasks; it does not do one-shot future work.
Autumn Harvest is for durable workflows with history and stronger orchestration
semantics — heavier than a one-shot timer. Delayed `#[job]` fills the gap
between "now" and "durable workflow".

### Admin dashboard

Delayed jobs appear in a distinct **Scheduled** list on `GET /admin/jobs`
showing each job's due time, and can be **canceled before they run**. (A job
that has already become due / started running cannot be canceled.)

## Backend selection (`autumn.toml`)

```toml
[jobs]
backend = "local"   # local | postgres | redis
workers = 2
max_attempts = 5
initial_backoff_ms = 250

[jobs.postgres]
# Reuses the configured [database] pool. No extra URL needed.
visibility_timeout_ms = 30000   # default: 30 000 ms

[jobs.redis]
url = "redis://127.0.0.1/"
key_prefix = "autumn:jobs"
visibility_timeout_ms = 30000
```

| Backend | Durable | Multi-replica safe | Extra infra |
|---|---|---|---|
| `local` | No | No (in-process) | None |
| `postgres` | Yes | Yes (SKIP LOCKED) | DB only — no Redis |
| `redis` | Yes | Yes | Redis |

- `local`: in-process channel, zero configuration. Jobs are lost on restart. Fine
  for development or single-process demos.
- `postgres`: Postgres-backed queue that reuses your existing `[database]` pool.
  Jobs survive restarts and are claimed atomically across replicas via
  `SELECT … FOR UPDATE SKIP LOCKED`. Requires the `db` feature and an
  `autumn migrate` run before the first worker starts.
- `redis`: Durable, Redis-backed queue for multi-replica workers. Higher
  throughput ceiling than `postgres` but adds Redis as an infrastructure dependency.

## Postgres delivery semantics

The Postgres backend provides **at-least-once delivery**. Each job is a row in
the `autumn_jobs` table. Workers claim a row atomically with
`UPDATE … WHERE id IN (SELECT … FOR UPDATE SKIP LOCKED)`, which prevents any
two replicas from claiming the same job simultaneously.

A claimed job's status is set to `running` with a `claimed_at` timestamp and a
`claimed_by` worker id. A maintenance loop running inside each worker process
requeues jobs whose `claimed_at` is older than `jobs.postgres.visibility_timeout_ms`.
Recovered stale claims consume another attempt and record a `last_error`
explaining the visibility timeout.

If a job exhausts `max_attempts`, its status is set to `failed`; it is no longer
retried.

Because the backend provides at-least-once delivery, handlers must be idempotent.
A slow worker that outlives the visibility timeout can overlap with a recovered
retry, so external side effects should use natural idempotency keys such as the
job id, a domain aggregate id, or a provider idempotency token.

## Redis delivery semantics

The Redis backend provides **at-least-once delivery**. A job is written as a
durable record, queued by id, atomically claimed into an in-flight set, and
acked only after the handler returns `Ok(())`.

If a worker crashes after claiming a job, the record remains in Redis. Another
worker requeues the stale claim after `jobs.redis.visibility_timeout_ms`.
Recovered stale claims consume another attempt and retain a `last_error`
explaining the visibility timeout. If the job has exhausted `max_attempts`, it
is moved to the dead-letter list instead of being requeued.

Because Redis uses at-least-once delivery, handlers must be idempotent. A worker
that is slow beyond the visibility timeout can overlap with a recovered retry,
so external side effects should use natural idempotency keys such as the job id,
domain aggregate id, or provider idempotency token.

## Retry/backoff and dead letters

- Jobs retry with exponential backoff (`initial_backoff_ms * 2^(attempt-1)`).
- Retries stop at `max_attempts` (job-level override or config default).
- Exhausted jobs are dead-lettered.
- Redis retries are scheduled in Redis before the worker moves on, so a crash
  during the backoff window does not drop the job.

## Job priorities

By default every job drains from a single FIFO queue, so a flood of low-value
work (analytics rollups, thumbnails, bulk re-indexing) can sit *ahead of*
latency-sensitive work like password-reset emails or payment-webhook fan-out.
Named queues fix this head-of-line blocking: route each job to a queue, and let
workers drain queues in priority order.

Tag a job's queue with `queue = "..."`. Jobs with no `queue` land on the
`"default"` queue, so apps that don't opt in behave exactly as before.

```rust,ignore
#[job(queue = "critical", max_attempts = 5)]
async fn send_password_reset(state: AppState, args: ResetArgs) -> AutumnResult<()> { … }

#[job(queue = "low")]
async fn rebuild_search_index(state: AppState, args: IndexArgs) -> AutumnResult<()> { … }

// No queue → the "default" queue.
#[job]
async fn send_receipt(state: AppState, args: ReceiptArgs) -> AutumnResult<()> { … }
```

Configure the worker drain order in `autumn.toml`. Two forms:

```toml
# Strict priority — workers always empty higher queues before lower ones.
# A single `critical` job jumps ahead of a 1,000-job `low` backlog.
[jobs]
queues = ["critical", "default", "low"]
```

```toml
# Weighted — fair draining that never starves a lower queue. Over a sustained
# mixed load each queue is served in proportion to its weight (here roughly
# 4 : 2 : 1), so `low` always makes forward progress even while `critical` has work.
[jobs.queues]
critical = 4
default = 2
low = 1
```

- **Strict** (`queues = [...]`) is the simple case: highest priority first, and a
  worker only pulls a lower queue when every higher queue is empty.
- **Weighted** (`[jobs.queues]` table) avoids starvation under sustained load:
  it uses smooth weighted round-robin, so each queue is the first choice in
  proportion to its weight over each cycle.

Routing is honored end-to-end on every backend (local, Redis, Postgres): the
queue is preserved through retries/backoff, dead-lettering, delayed enqueues, and
`enqueue_after_commit`. The actuator/admin job view shows each job's queue.

If a job declares a `queue` that is **not** in the configured drain list, that is
a loud, documented condition — it is logged at startup (`WARN`) and the queue is
appended at lowest priority so the job still drains instead of silently stalling.
Add the queue to `[jobs] queues` to control its priority.

> Out of scope (separate follow-ups): per-job-instance dynamic priority at
> enqueue time, and per-queue concurrency caps / dedicated worker pools.

## Uniqueness and concurrency limits

`#[job]` can declare dedup and in-flight caps directly, so double-submits and
bursts cannot duplicate side effects or overwhelm downstream systems — no
hand-rolled advisory locks in job bodies.

```rust,ignore
// At most one identical sync in flight: a burst of N identical enqueues
// runs exactly once. The key defaults to a stable hash of the full args.
#[job(unique)]
async fn sync_search_index(state: AppState, args: SyncArgs) -> AutumnResult<()> { … }

// Key by selected args fields, and cap simultaneous executions per account.
#[job(unique_by = "account_id", concurrency = 1, concurrency_key = "account_id")]
async fn recalculate_account(state: AppState, args: RecalcArgs) -> AutumnResult<()> { … }

// Debounce: coalesce repeat enqueues for 60s from the first enqueue,
// even after the job completed.
#[job(unique_for_ms = 60_000)]
async fn rebuild_report(state: AppState, args: ReportArgs) -> AutumnResult<()> { … }
```

Attributes:

| Attribute | Meaning |
|---|---|
| `unique` | Dedupe on a stable hash of the full args payload. |
| `unique_by = "a, b"` | Dedupe on the listed args fields (implies `unique`). |
| `unique_window = "running"` | Default: key held while the job is pending **or** running; released when it settles. |
| `unique_window = "pending"` | Key released when execution starts, so a new instance may queue while one runs. |
| `unique_for_ms = N` | TTL window: key held for `N` ms from enqueue (and while in flight on Postgres), even past completion. Mutually exclusive with `unique_window`. |
| `concurrency = N` | At most `N` simultaneously-executing jobs of this type. |
| `concurrency_key = "field"` | Scope the limit per distinct value of this args field. |

Semantics:

- A coalesced enqueue is a **no-op `Ok(())`**; it is counted as
  `total_deduplicated` in `/actuator/jobs` and recorded with the
  `deduplicated` job-admin status.
- Jobs over the concurrency cap **wait** (they stay enqueued/parked and run
  when a slot frees) — they are never dropped.
- Keys and slots are released on success, terminal failure, **and worker
  crash**: Postgres ties them to row status recovered by the visibility
  timeout; Redis settles them in the claim-validated transition and
  stale-recovery scripts, with a TTL backstop on lock keys.
- Enforcement is **distributed-safe** across replicas on the durable
  backends: Postgres uses a partial unique index plus `ON CONFLICT DO
  NOTHING` for dedup and (only when a limited job is registered) a
  transaction-scoped advisory lock around claims; Redis uses `SET NX PX`
  locks and atomic Lua claim/settle scripts.
- With neither attribute set, behavior is unchanged: no dedup and unbounded
  per-type concurrency.
- Retries keep a `running`-window key held (the job is still in flight) and
  re-acquire a `pending`-window key while waiting out the backoff; the
  concurrency slot is released during the backoff either way.
- After a pending-window job's first execution attempt, dedup is **best
  effort**: the key is released when execution starts (that is the window's
  contract), so a duplicate accepted while the job runs legitimately holds
  the key, and a retry or crash-recovered attempt then waits as pending
  without it. Workloads that must never overlap should use the default
  `running` window, which holds the key until the job settles.
- Operator actions respect uniqueness: canceling an enqueued job (including
  one parked behind a concurrency slot) releases its key immediately, and
  retrying a failed unique job re-takes the key — or fails with a clear
  conflict error when an equivalent job is already pending or running.
- On Redis, pending/running unique locks carry a 24-hour crash backstop TTL
  that is refreshed every time the job is claimed, retried, or recovered, so
  only a job left completely untouched for a full day can lose its lock.
- The Postgres backend needs the additive `autumn migrate` migration that
  adds the nullable `unique_key`/`unique_window`/`concurrency_key`/
  `concurrency_limit` columns; rows and jobs without them behave as before.

## Tracked jobs and progress polling

Plain `enqueue` is fire-and-forget — there's no handle, no progress, and
nowhere for the caller to check "is it done yet?". `enqueue_tracked` fixes
that: it returns a handle carrying a public, unguessable token, distinct
from the internal job id, that the browser can poll at a built-in status
route while the job reports progress from the inside.

### Enqueue a tracked job

```rust,ignore
let handle = ExportOrdersJob::enqueue_tracked(ExportArgs { account_id: 42 }).await?;
// handle.token is the raw, unguessable token — deliver it to the caller.
// handle.status_path() is "/_autumn/jobs/{token}".
```

By default the token is an **anonymous capability**: anyone holding it can
poll the status. To bind status access to the caller's session/user instead,
use `enqueue_tracked_for` with an owner derived from the current session:

```rust,ignore
use autumn_web::job::TrackedJobOwner;

let owner = TrackedJobOwner::from_session(&session, &state).await;
let handle = ExportOrdersJob::enqueue_tracked_for(args, owner).await?;
```

A request whose session doesn't match the bound owner gets the identical
`404` an unknown token would — the route is never an existence/ownership
oracle.

`enqueue_tracked`/`enqueue_tracked_for` wrap your `Args` in an internal
envelope under a reserved top-level field named `__autumn_tracked`. Don't
give a job's `Args` struct a field with that exact name — `enqueue`/
`enqueue_in`/`enqueue_at` and their `on_conn` variants reject a payload
shaped that way with a `400` rather than risk it being misread as a tracked
envelope.

### Report progress from inside the handler

Add a third `JobContext` argument to a `#[job]` handler to opt into
progress reporting; the two-argument form keeps working unchanged:

```rust,ignore
#[job(name = "export_orders")]
async fn export_orders(
    state: AppState,
    args: ExportArgs,
    ctx: JobContext,
) -> AutumnResult<()> {
    ctx.set_progress(0, Some("Starting export")).await?;

    // ... do the work, reporting progress as it goes ...
    ctx.set_progress(50, Some("Rows 2500/5000")).await?;

    // On success, the JSON result is whatever the caller wants back —
    // e.g. a link to the finished file.
    ctx.set_result(serde_json::json!({ "download_url": "/blob/orders-42.csv" }));
    Ok(())
}
```

If the handler returns `Err`, the job retries as usual (`max_attempts`,
backoff); only the **final** failed attempt (or a panic, which always
dead-letters) settles the tracked record to `failed`. Call
`ctx.set_user_error("...")` before returning `Err` to control the message
shown to the caller — otherwise a generic "The job failed." is recorded (the
raw error is never leaked to the tracked-status response).

### Poll the status

`GET /_autumn/jobs/{token}` (mounted automatically; disable with
`jobs.tracking.route_enabled = false`) is content-negotiated:

- **API clients** (no `Accept: text/html`, no `HX-Request`) get JSON:

  ```json
  {"status": "running", "progress": 50, "message": "Rows 2500/5000", "result": null, "error": null}
  ```

- **htmx requests** (`HX-Request: true`) or a browser `Accept: text/html`
  get a self-polling fragment. While the job is pending/running, the
  fragment carries `hx-get={path} hx-trigger="every 2s" hx-swap="outerHTML"`,
  so it keeps re-fetching and replacing itself with zero app-authored JS.
  Once the job reaches a terminal state, the fragment drops every `hx-*`
  attribute — htmx has nothing left to poll — and renders either a download
  link (when the result carries a `download_url`) or the failure message.

Embed the poll target directly in a page:

```rust,ignore
html! {
    div hx-get=(handle.status_path()) hx-trigger="load" hx-swap="outerHTML" {
        "Starting export…"
    }
}
```

### Result store TTL and backends

Progress/result records expire `jobs.tracking.ttl_secs` after their last
write (default `86400`, 24h). The record store follows whichever job
backend is configured — `local` and `redis` use an in-memory or Redis-backed
store respectively, `postgres` uses the `autumn_job_tracking` table (see
[Migration notes](#migration-notes)) — so a tracked job's status composes
with the backend an app already runs, with no extra setup. Expired records
are invisible to reads/writes immediately on all three stores; each also
actually frees the expired record so long-running processes don't
accumulate one dead entry per tracked job forever: the in-memory store
sweeps them out opportunistically (amortized across `create` calls), a
Postgres background sweep runs every 5 minutes to `DELETE` expired rows,
and Redis expires keys natively via `EX`.

### Async CSV export, end to end

The synchronous `GET /{plural}/export.csv` admin route runs inline on the
request thread — fine for small tables, but a 50k-row export blocks the
worker and risks tripping a proxy idle timeout. A tracked job moves that
work off the request thread:

```rust,ignore
use autumn_web::data::csv::export_csv;
use autumn_web::prelude::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportOrdersArgs {
    pub account_id: i64,
}

#[job(name = "export_orders")]
async fn export_orders(
    state: AppState,
    args: ExportOrdersArgs,
    ctx: JobContext,
) -> AutumnResult<()> {
    let repo = OrderRepository::from_state(&state);
    let orders = repo.for_account(args.account_id).await?;

    let total = orders.len();
    let mut buffer = Vec::new();
    for (i, chunk) in orders.chunks(500).enumerate() {
        export_csv(chunk.iter().cloned(), &mut buffer)?;
        let done = ((i + 1) * 500).min(total);
        ctx.set_progress(
            u8::try_from(done * 100 / total.max(1)).unwrap_or(100),
            Some(&format!("Rows {done}/{total}")),
        )
        .await?;
    }

    let url = state.storage().put("exports/orders.csv", buffer).await?;
    ctx.set_result(serde_json::json!({ "download_url": url }));
    Ok(())
}

// Kick it off from a request handler and hand back a poll target — the
// initiating request returns immediately instead of blocking on the export.
#[post("/orders/export")]
async fn start_export(state: AppState, session: Session) -> AutumnResult<Markup> {
    let owner = TrackedJobOwner::from_session(&session, &state).await;
    let handle = ExportOrdersJob::enqueue_tracked_for(
        ExportOrdersArgs { account_id: 1 },
        owner,
    )
    .await?;

    Ok(html! {
        div hx-get=(handle.status_path()) hx-trigger="load" hx-swap="outerHTML" {
            "Export starting…"
        }
    })
}
```

The browser gets a progress bar within milliseconds and a download link the
moment the job finishes — no hand-written status table, token, or polling
endpoint anywhere in app code.

### Known limitation: tracked status vs. the durable queue record

On the Redis and Postgres backends, a job's tracked status (`succeeded`/
`failed`) is settled *before* that backend's own success/dead-letter
acknowledgement is written. In the rare case where that ack later fails or
is skipped (e.g. the claim changed because another worker recovered it as
stale in between), the tracked status can briefly report a terminal state
the durable queue record hasn't actually reached — a poller may stop
watching a moment before the durable backend finishes catching up, which it
does automatically on its own retry/recovery path. This window only opens
on an ack failure, not on ordinary success/failure/retry. Treat the tracked
status as a progress/UX signal for the caller, not as the source of truth
for whether a job will run again — use the admin dashboard
([Observability](#observability)) or `JobAdminBackend` for that.

### Known limitation: stale progress writes on the Redis/Postgres stores

`mark_running`/`set_progress` intentionally no-op once a tracked record is
already terminal, so a stray write from an abandoned attempt can't overwrite
a legitimate final result — but on the Redis and Postgres tracking stores
that guard is evaluated against the value read at the *start* of that write
(read the record, decide, write it back), not atomically at write time. If a
worker's claim times out and it keeps running past that point while a
replacement worker claims and completes the same job, the original worker's
next progress write can land *after* the replacement's terminal write and
briefly clobber it, because its in-memory guard was evaluated against the
`running` record it read before the replacement settled. The record
self-corrects on the next terminal write (the replacement's own retry
path settles it, or TTL expiry clears it), so this is a transient display
glitch, not a lost result — the queue's own durable state (visible via the
admin dashboard) is never affected. Closing this fully means moving the
terminal-status guard into the durable write itself (a Lua script for
Redis, a conditional `UPDATE` for Postgres — the same compare-and-swap
approach `JobTrackingStore::reset_for_retry` already uses to make an
operator retry race-safe); tracked as a follow-up rather than folded into
this change.

## Observability

Mount `autumn-admin-plugin` to get the built-in operator dashboard at
`GET /admin/jobs` (or the plugin prefix you choose). It lists enqueued, running,
recently completed, and failed jobs with retry/discard/cancel actions. See the
[Operating Background Jobs](operating-background-jobs.md) guide for dashboard
setup, action semantics, and bounded refresh behavior.

`GET /actuator/jobs` returns per-job:

- `queued`
- `in_flight`
- `blocked_on_concurrency`
- `total_successes`
- `total_failures`
- `dead_letters`
- `total_deduplicated`
- `last_error`

For Redis deployments these counters are process-local operational telemetry,
not a strongly consistent Redis aggregate. They remain useful for seeing queued,
in-flight, success, retry/failure, and dead-letter activity observed by the
replica serving the actuator request.

## Migration notes

When using `jobs.backend = "local"` or `jobs.backend = "redis"`, no SQL migration
is required.

When using `jobs.backend = "postgres"`, the `autumn_jobs` table must exist before
workers start. Run your app migrations as a one-shot `autumn migrate` job before
scaling web and worker replicas:

```bash
autumn migrate   # creates autumn_jobs, autumn_job_tracking, your domain tables, etc.
```

The migration is bundled with the framework and is applied automatically by
`autumn migrate` as long as the `db` feature is enabled. `enqueue_tracked`
works the same way regardless of `jobs.backend` — the framework migration
also creates `autumn_job_tracking`, the table the Postgres-backed tracking
store uses (see [Tracked jobs and progress polling](#tracked-jobs-and-progress-polling)).

---

## Transactional enqueue

When a job must be coordinated with a database write, choose the API based on
which guarantee you need:

- `enqueue_after_commit` prevents jobs for rolled-back data on any backend, but
  the post-commit callback is process-local and can be lost if the process exits
  after commit.
- `enqueue_in_tx` / `enqueue_on_conn` on the Postgres backend write the job row
  in the same transaction as the domain row, which is the crash-safe handoff.

### `enqueue_after_commit` — any backend

`autumn_web::job::enqueue_after_commit` registers the enqueue as an
after-commit callback inside the surrounding `db.tx` block. The job is only
dispatched if the transaction commits. Works with every job backend.

This is not crash-safe delivery. If the process exits after the transaction
commits but before the callback runs, no job may be recorded. Use this for
rollback coordination across backends, not as a durable outbox substitute.

```rust,no_run
use autumn_web::prelude::*;
use scoped_futures::ScopedFutureExt;

async fn create_order(mut db: Db) -> AutumnResult<()> {
    db.tx(|conn| async move {
        // ... INSERT order ...

        // Enqueued only after INSERT commits; dropped if the tx rolls back.
        // For crash-safe Postgres handoff, use enqueue_in_tx instead.
        autumn_web::job::enqueue_after_commit("ship_order", &args).await?;

        Ok::<_, AutumnError>(())
    }.scope_boxed())
    .await
}
```

### `enqueue_in_tx` / `enqueue_on_conn` — Postgres backend only

On the Postgres backend the job row can live in the **same transaction** as
the domain row. Both commit or roll back together, avoiding the post-commit
process crash window at the cost of being limited to the `postgres` backend.

```rust,no_run
use autumn_web::prelude::*;
use scoped_futures::ScopedFutureExt;

async fn create_order(mut db: Db) -> AutumnResult<()> {
    db.tx(|conn| async move {
        // ... INSERT order using conn ...

        // Job row written into the same transaction.
        autumn_web::job::enqueue_in_tx("ship_order", &args, conn).await?;

        Ok::<_, AutumnError>(())
    }.scope_boxed())
    .await
}
```

See [Transactions -> after_commit](transactions.md#after_commit--post-commit-process-local-callbacks)
for a full comparison of the two strategies and guidance on when to use each.

For cloud-native rollout run the migration job first, then start web and workers.
