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
backend = "local"   # local | postgres | redis | sqlite
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

[jobs.sqlite]
# Reuses the configured [database] pool. No extra URL needed.
visibility_timeout_ms = 30000   # default: 30 000 ms
poll_interval_ms = 250          # default: 250 ms
```

| Backend | Durable | Shared by | Extra infra |
|---|---|---|---|
| `local` | No | Nothing (in-process) | None |
| `postgres` | Yes | Every replica (SKIP LOCKED) | DB only — no Redis |
| `redis` | Yes | Every replica | Redis |
| `sqlite` | Yes | Every process on the host | DB only — no Redis |

- `local`: in-process channel, zero configuration. Jobs are lost on restart. Fine
  for development or single-process demos.
- `postgres`: Postgres-backed queue that reuses your existing `[database]` pool.
  Jobs survive restarts and are claimed atomically across replicas via
  `SELECT … FOR UPDATE SKIP LOCKED`. Requires the `db` feature and an
  `autumn migrate` run before the first worker starts.
- `redis`: Durable, Redis-backed queue for multi-replica workers. Higher
  throughput ceiling than `postgres` but adds Redis as an infrastructure dependency.
- `sqlite`: durable queue in the app's own SQLite file, for the single-host
  SQLite tier. Requires the `sqlite` cargo feature. The runtime creates the
  table itself, so no migration is needed. See
  [SQLite delivery semantics](#sqlite-delivery-semantics).

## Web and worker process roles

The same binary can run in one of three **process roles**, so you can scale the
HTTP tier separately from the background-work tier without touching app code. No
handler, `#[job]`, or `#[scheduled]` definition changes — only config or a flag.

| Role | Serves user routes | Runs workers + scheduler | Enqueues jobs |
|---|---|---|---|
| `combined` (default) | Yes | Yes | Yes |
| `web` | Yes | **No** | Yes |
| `worker` | **No** (probes/actuator only) | Yes | Yes |

- **`combined`** is the default and preserves today's single-process behavior:
  it serves HTTP, drains `#[job]` workers, and runs the `#[scheduled]` cron.
  Existing apps and `autumn dev` need zero changes.
- **`web`** serves HTTP and still **enqueues** jobs, but runs no workers and no
  scheduler, so background work never competes with request handling on a web
  replica.
- **`worker`** runs the workers and the cron scheduler against the shared
  durable backend and does **not** serve user routes — but it still binds the
  HTTP listener to expose only the liveness/readiness probes (`/live`, `/ready`,
  `/startup`, `/health`) and the actuator (`/actuator/*`, including
  `/actuator/jobs`), so an orchestrator can supervise it.

### Selecting a role

Three equivalent mechanisms:

```bash
# Environment variable — the usual container knob.
AUTUMN_ROLE=web     # or: worker | combined
```

```toml
# autumn.toml — top-level key.
role = "worker"     # or: web | combined
```

```bash
# CLI flag on the production server command.
autumn serve --role worker   # or: --role web
```

### Self-gating app-owned background work

The framework already uses the resolved role to gate the `#[job]` runtime, the
`#[scheduled]` cron scheduler, and commit-hook workers — but that gate only
covers **framework-managed** work. If your app wires its own background loop in
an `on_startup` hook (a poller, a warm-cache refresher, a queue consumer you
manage yourself), that loop runs on **every** replica unless you gate it too.

The resolved role is available as a first-class accessor on `AppState`, so you
can self-gate without re-reading `AUTUMN_ROLE` by hand. It is the same value the
framework resolved (config + `AUTUMN_ROLE` env override + `--role` flag) — one
source of truth, no second parse:

```rust
use autumn_web::{AppState, ProcessRole};

// In an on_startup hook, plugin, state_initializer, or handler:
fn start_background_work(state: &AppState) {
    if state.role().runs_workers() {
        // Only replicas that run workers (combined or worker) spin up the loop.
        tokio::spawn(my_embedded_worker(state.clone()));
    }

    if state.role().serves_http() {
        // Web-facing warmups belong on replicas that serve user routes.
        warm_render_caches(state);
    }
}
```

`state.role()` returns a `ProcessRole` (exported at `autumn_web::ProcessRole`);
its `serves_http()` and `runs_workers()` predicates map roles to tiers exactly
as the table above does. The value is reachable from `state_initializer`,
`on_startup`/`on_shutdown` hooks, plugins, and request handlers.

> **Footgun: `AUTUMN_ROLE` alone does not gate app-owned background work.**
> Setting `AUTUMN_ROLE=web` stops the framework's `#[job]`/`#[scheduled]`
> workers, but a background task you `tokio::spawn` yourself in `on_startup` has
> nothing to do with that gate — it will still run on your web replicas and
> double-process work alongside the worker tier. Wrap app-owned loops in
> `if state.role().runs_workers() { … }` so they land only where you intend.

Custom or named roles beyond `combined`/`web`/`worker` are not supported today;
if you need finer-grained placement, use per-queue worker pinning (#1623)
combined with app-level `state.role()` gating rather than inventing new role
names.

### Split roles require a durable backend

A split web/worker topology **requires a durable jobs backend**
(`jobs.backend = "postgres"`, `"redis"`, or `"sqlite"`). The default `local`
backend is an in-process, in-memory queue: a `web` replica would enqueue into
its own memory, where no separate `worker` replica can ever drain it.

`sqlite` qualifies because the queue is a table both processes open — but only
while they run on the **same host**, which a SQLite deployment does by
definition, and only against a **file-backed** database. An in-memory target
(`sqlite::memory:`, `file::memory:`, `?mode=memory`) is private to each process,
so a split role on one is refused at boot and flagged by
`autumn doctor --strict`.

Autumn rejects this combination at startup — a `web` or `worker` role on the
`local` backend exits with a clear error rather than silently dropping work —
and `autumn doctor --strict` reports it as a **Fail**. `combined` on `local` is
always fine, because one process both enqueues and drains.

```toml
[jobs]
backend = "postgres"   # or "redis" / "sqlite"; required once role is "web" or "worker"
```

### Graceful drain on workers

A `worker`-role process joins the same shutdown sequence as a web replica. On
SIGTERM it stops claiming new jobs, gives in-flight jobs the configured drain
window (`server.shutdown_timeout_secs` / `server.prestop_grace_secs`) to finish
or be released back to the queue, then exits cleanly. Its `/ready` probe flips
to `503` during the drain, so an orchestrator supervises a rolling worker deploy
exactly as it would a web deploy. See
[Rolling Deploy Lifecycle](cloud-native.md#rolling-deploy-lifecycle) for the
full phase ordering.

### Where job execution shows up

`/actuator/jobs` and the admin dashboard attribute each job's execution to the
process that actually ran it. In a split topology that is always a `worker`
process — `web` replicas only enqueue, so handler execution, in-flight counts,
and last-error data surface on the worker tier.

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

## SQLite delivery semantics

The SQLite backend provides **at-least-once delivery** with the same row
lifecycle as Postgres. Each job is a row in an `autumn_jobs` table in the app's
own database file, so work survives a restart with no Redis and no Postgres.

SQLite serializes writers, so a worker claims a row with a single statement —
`UPDATE … WHERE id = (SELECT … ORDER BY run_at LIMIT 1) RETURNING …` — which is
the single-host analog of `FOR UPDATE SKIP LOCKED`. Two workers can never claim
one row.

A claimed row is `running` with a `claimed_at` timestamp and a `claimed_by`
worker id. A maintenance loop re-enqueues rows whose `claimed_at` is older than
`jobs.sqlite.visibility_timeout_ms`, at start and on an interval, so a crash
mid-job loses nothing. A recovered claim consumes another attempt and records a
`last_error`. A job that exhausts `max_attempts` becomes `failed` and is not
retried.

There is no `LISTEN`/`NOTIFY`. An enqueue in the same process wakes a worker
directly; work another process enqueued is seen within
`jobs.sqlite.poll_interval_ms` (default 250ms). Lower it for latency, raise it
to cut idle wakeups.

Terminal rows are pruned by the runtime, not by `autumn db retention`, whose
sweep is Postgres-only. Set `retention.job_history` to bound the table; leave it
unset and history is kept forever, exactly as on Postgres.

The runtime creates the table and its indexes at start, because framework
migrations are Postgres SQL. Nothing to run by hand.

Because delivery is at-least-once, handlers must be idempotent — the same rule
as every other durable backend.

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

> **Role-based scaling** — running a separately scaled worker tier that drains
> all configured queues — is covered in
> [Web and worker process roles](#web-and-worker-process-roles).

## Per-queue worker pools, caps, and pinning

By default every worker process draws from one shared pool of `jobs.workers`
slots and drains all configured queues. Three config-only knobs (no application
code changes) let you carve that pool up per queue and dedicate worker processes
to specific queues.

### Dedicated capacity and per-queue caps

Extend the weighted `[jobs.queues]` table so a queue's value is a table instead
of a bare weight. Two per-queue controls are available:

- `reserved = N` — **dedicated slots** that no other queue may ever consume. A
  flood on another queue can never starve this one: `N` of the process's worker
  slots are always available to it.
- `concurrency = N` — a **hard cap**: this queue may occupy at most `N` of the
  process's worker slots at once, so a bulk queue can never take more than its
  configured share.

```toml
[jobs]
workers = 8

[jobs.queues]
# `critical` keeps 2 slots reserved for it at all times — a slow flood on
# `bulk` can never delay a password-reset email.
critical = { weight = 4, reserved = 2 }
# `bulk` is capped at 4 of the 8 slots, so re-indexing never monopolizes the
# process.
bulk = { weight = 1, concurrency = 4 }
# A bare integer is still just a weight (no cap, no reservation).
default = 2
```

The bare-integer form (`critical = 4`) and the strict array form
(`queues = ["critical", "default", "low"]`) keep working unchanged; a queue with
neither `reserved` nor `concurrency` behaves exactly as before. Slots are
accounted **per process**: total capacity is always `jobs.workers`, and the
reserved/cap rules only redistribute how those slots are shared between queues.

The accounting is enforced on every backend (local, Redis, Postgres): before a
worker claims a job it restricts itself to the queues that currently have a free
slot, so a queue at its cap — or whose only free slots are reserved for another
queue — is skipped rather than blocked behind.

### Pinning a worker tier to a subset of queues

Combine per-queue pools with [process roles](#web-and-worker-process-roles) to
dedicate an entire worker tier to a subset of queues. Set `jobs.pin` (or the
`AUTUMN_JOBS__PIN` environment variable, comma-separated) to the queues that
process should claim:

```toml
# A worker replica dedicated to latency-sensitive work.
# `role` is a TOP-LEVEL key (AutumnConfig.role) and must appear before any
# `[table]` header — equivalently, set `AUTUMN_ROLE=worker` in the environment.
role = "worker"

[jobs]
backend = "redis"
pin = ["critical"]
```

```bash
# Same thing via env — handy for a separate deployment of the same image.
AUTUMN_JOBS__PIN=bulk,default
```

```bash
# Or as a flag, when one image is started under several roles from a script.
# `--pin` is repeatable and comma-separated, and sets `AUTUMN_JOBS__PIN` for the
# app process; `autumn serve restart` restores it.
autumn serve --role worker --pin critical
autumn serve --role worker --pin bulk --pin default
```

A pinned process **never** claims jobs from queues outside its subset, on both
the Postgres and Redis backends. Weighted/strict ordering is preserved *within*
the pinned subset. An empty/unset `pin` (the default) keeps today's behavior:
the process drains every configured queue from the single shared pool.

### Zero-coverage guard

If pinning leaves a configured queue with **no** worker coverage anywhere —
e.g. you pin every worker to `critical` but still enqueue to `bulk` — those jobs
would silently accumulate. Autumn diagnoses this loudly:

The **runtime startup warning is the authoritative check**: a worker process
that pins to a subset logs a `WARN` at startup naming the configured queues it
does not cover (and an `ERROR` if the pin matches no queue on its effective
schedule at all, so the process would claim nothing). This runs *inside the
app*, against the job registry and the real effective schedule, so it knows both
the `[jobs.queues]` config **and** any queues declared solely via
`#[job(queue = "…")]` — and diagnoses a genuinely uncovered queue loudly at
boot.

`autumn doctor` reports coverage as a `jobs_queue_coverage` check. What that
check can *prove* — report only, or fail the run — depends on how much of the
deployment you have declared.

**Without a declared topology it only reports.** Doctor is config-only and
per-process: it sees **one** process, so it cannot know what sibling worker
tiers drain — a multi-tier deployment where each pinned tier omits queues
covered by other tiers (one process `AUTUMN_JOBS__PIN=critical`, another
`AUTUMN_JOBS__PIN=bulk,default`) is perfectly legitimate. It also inspects only
`[jobs.queues]` (plus the implicit `default` queue), so it cannot see a queue
declared solely via `#[job(queue = "…")]`, which the runtime appends to the
effective schedule and drains. Any hard-fail asserted from that vantage point
would false-positive on a valid deployment, so the check passes and prints what
this tier claims, which configured queues it does not claim, and any pinned
queues absent from `[jobs.queues]`.

**Declare the fleet topology and doctor can hard-fail.** Give doctor every
worker tier's pin under `[jobs.fleet]` and it reasons about the *union* of what
the fleet drains instead of one process, which makes a zero-coverage gap
provable:

```toml
[jobs.queues]
critical = { weight = 4, reserved = 2 }
bulk = { weight = 1, concurrency = 4 }
default = 2

[jobs.fleet]
# One entry per worker tier, each holding that tier's `jobs.pin`. This must list
# EVERY tier you actually run — see the warning below.
# An empty entry (`[]`) is an unpinned tier that drains every queue.
tiers = [["critical"], ["bulk", "default", "thumbnails"]]

# Optional — teach doctor about queues declared in code but absent from
# `[jobs.queues]` (here, a `#[job(queue = "thumbnails")]`), so they count toward
# coverage too. List them inline…
declared_queues = ["thumbnails"]
```

…or, better, point at a manifest the app emits, which is the ground-truth set
rather than a list you maintain by hand. Generate it with
`autumn jobs manifest <path>` (it runs your app binary to dump the queues it
really drains) and reference it instead of `declared_queues` — when both are
set, `manifest` wins and `declared_queues` is never read:

```toml
[jobs.fleet]
tiers = [["critical"], ["bulk", "default", "thumbnails"]]
manifest = "target/jobs-manifest.toml"
```

With a topology declared, `autumn doctor` **fails** — exit 1, in every mode, not
only under `--strict` — when some queue that must be drained is claimed by no
tier anywhere, naming the uncovered queues. (`--strict` escalates *warnings*; a
failed check is fatal regardless.)

`manifest` and `declared_queues` can only *add* to the set of queues that must
be covered, so an incomplete or missing manifest never manufactures a failure —
it just leaves doctor blind to the queues it did not see.

> **`tiers` must be exhaustive.** It is the one input where under-declaring is
> unsafe: doctor can only reason about the tiers you list, so omitting a tier
> that really runs — especially an unpinned one, which would make coverage
> total — reports a gap that does not exist. Treat `[jobs.fleet] tiers` as the
> checked-in description of the whole fleet, and update it when you add or
> remove a worker tier.

`[jobs.fleet]` is purely declarative. A running process never acts on it — it
describes the *other* tiers, which no single process can observe — so it changes
no runtime behavior, and an app that declares nothing keeps today's behavior.

Absent that declaration, ensuring every queue is drained by some tier remains
the operator's responsibility: make sure some process (an unpinned worker tier,
or one pinned to those queues) covers every queue you enqueue to.

### Observability

The actuator jobs endpoint (`<actuator-prefix>/jobs`) reports per-queue queue
depth and the age of the oldest still-waiting job under a `queues` key, in
addition to the existing per-job-type gauges under `jobs`:

```json
{
  "jobs": { "send_password_reset": { "queued": 0, "in_flight": 1, "...": 0 } },
  "queues": { "critical": { "depth": 3, "oldest_waiting_age_ms": 1200 } }
}
```

On the durable backends (Postgres/Redis) these gauges are **not** per-process
approximations: a periodic survey of the durable store wholesale-replaces this
process's local enqueue marks each tick (#1752), so an enqueue-only `web`
replica reports the true shared backlog, and a queue absent from the latest
survey resets to `depth` 0 rather than leaking a stale one. On the in-process
`local` backend there is only one process, so the local marks *are* the whole
picture.

> Still out of scope (separate follow-up): per-job-instance dynamic priority at
> enqueue time.

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
[Migration notes](#migration-notes)), and `sqlite` uses the same table in the
app's own database file — so a tracked job's status composes with the backend
an app already runs, with no extra setup. Expired records are invisible to
reads/writes immediately on every store; each also actually frees the expired
record so long-running processes don't accumulate one dead entry per tracked
job forever: the in-memory store sweeps them out opportunistically (amortized
across `create` calls), the Postgres and SQLite background sweeps run every 5
minutes to `DELETE` expired rows, and Redis expires keys natively via `EX`.

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

### Known limitation: a `JobInterceptor` erroring after `next` still settles the tracked record as failed

`enqueue_tracked`/`enqueue_tracked_for` treat any `Err` from the enqueue call
as "never delivered" and settle the tracked record to `failed`. If an app
installs a `JobInterceptor` whose `intercept_enqueue`
successfully awaits `next` (so the job *was* delivered to the backend) and
then its own post-`next` logic returns an error — e.g. an audit-log call
that fails — that error is indistinguishable, from this call site alone,
from a delivery failure: the tracked record settles to `failed` and the
caller sees an error even though the job will still run. A caller that
retries on that error risks enqueueing a duplicate. Closing this requires
`JobClient::enqueue_with_outcome` to expose whether the backend write
actually happened on its error path too (it currently tracks this
internally via a `started` flag but only uses it to select the `Ok`
variant), which is a signature change shared by every enqueue path — plain
`enqueue()` has the identical gap for the same reason — not just the
tracked ones; tracked as a follow-up. In the meantime, avoid returning an
error from `intercept_enqueue` after `next` has resolved successfully.

### Known limitation: the built-in job-status route can collide with a user route

`GET /_autumn/jobs/{token}` mounts by default (`jobs.tracking.route_enabled
= true`) for every app that registers jobs, merged after user routes are
already mounted. If an app happens to define its own route at that exact
path, `try_build_router_inner` panics on the resulting Axum overlapping-route
error at startup rather than surfacing a typed, recoverable collision error
(the framework's other reserved-but-default-on routes — the mail
unsubscribe endpoint, mail previews — have the same gap). `/_autumn/` is a
framework-reserved path prefix, so this should not come up in practice;
set `jobs.tracking.route_enabled = false` if a conflict is ever hit.
Building a general framework-route-vs-user-route collision preflight
(mirroring the existing OpenAPI/MCP-vs-everything check) is a broader
router-building change than this PR's scope; tracked as a follow-up.

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

When using `jobs.backend = "sqlite"`, the runtime creates its own `autumn_jobs`
table at start; no migration step is needed.

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
