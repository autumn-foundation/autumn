# Cloud-Native Autumn

This guide is the blunt version: Autumn can run as a single-process monolith
with almost no config, but that does not automatically make every default safe
for multi-replica production.

## Baseline

A production-minded Autumn deployment should usually have all of these in
place:

1. `AUTUMN_PROFILE=prod`
2. `/live`, `/ready`, and `/startup` connected to platform probes
3. OTLP telemetry enabled, or at minimum JSON logs
4. Redis-backed sessions if more than one web replica will serve the same users
5. SMTP-backed mail if the app sends account or notification email
6. an explicit migration job before web replicas start
7. a clear choice between `#[scheduled]` and Harvest for background work

When a single primary (plus replicas) stops being enough for writes, see
the [Horizontal Sharding guide](sharding.md) — `[[database.shards]]`
routes tenant data across multiple Postgres databases while framework
state stays on the control role.

## What `autumn new` Gives You

The scaffold now includes:

- `Dockerfile` with a multi-stage build
- `.dockerignore`
- commented probe, telemetry, and Redis session examples in `autumn.toml`

That is container scaffolding, not a full cluster deployment. You still need to
decide your runtime topology.

## Probes

Autumn mounts:

- `/live`
- `/ready`
- `/startup`
- `/health`

Recommended use:

- liveness probe -> `/live`
- readiness probe -> `/ready`
- startup probe -> `/startup`

Do not point all three at `/health` just because it was easy in older apps.

## Telemetry

Use the `[telemetry]` section or the `AUTUMN_TELEMETRY__*` environment
variables to declare service metadata and an OTLP endpoint.

Example:

```toml
[telemetry]
enabled = true
service_name = "bookmarks"
service_namespace = "apps"
environment = "production"
otlp_endpoint = "http://otel-collector:4317"
protocol = "Grpc"
```

If you are not ready for OTLP yet, force `log.format = "Json"` so your logs are
at least machine-readable.

### What you get automatically

With the `telemetry-otlp` cargo feature enabled and `telemetry.enabled = true`:

- **W3C Trace Context propagation.** Incoming `traceparent` / `tracestate`
  headers are extracted and attached to a server span; the current context
  is injected back into the response headers so callers can continue the
  trace. No manual middleware setup required.
- **Scheduled task traces.** Each invocation of a `#[scheduled]` function
  runs inside a fresh root span (`scheduled_task` / `task=<name>`) so
  every run shows up as its own trace in your APM.
- **Database spans.** The `Db` extractor opens a `db.connection.acquire`
  span tagged with `db.system=postgresql` whose scope covers the lifetime
  of the pooled connection — Diesel activity performed through it appears
  as a child of the request span in Jaeger / Tempo / Datadog.
- **Job and mailer trace propagation.** `#[job]` and `#[mailer]` boundaries
  now carry the W3C `traceparent` / `tracestate` into their durable payloads.
  See below.

### Trace context across job and mailer boundaries

By default a distributed trace dies at any queue boundary: the HTTP request
span ends, the worker picks up the job on a different replica minutes later,
and your APM shows two disconnected traces with no link between them.

With `telemetry-otlp` enabled, Autumn serializes the active `traceparent` (and
optional `tracestate`) into every job payload at enqueue time, and then
re-parents the `job.execute` span to that context when the job is dequeued:

```
HTTP request span (web replica)
 └─ enqueue SendWelcomeEmail
     └─ job.execute SendWelcomeEmail   ← worker replica, seconds later
         └─ … your job logic …
```

This works across all three backends:

| Backend | Where the context is stored |
|---|---|
| `local` | In-process channel (`QueuedJob.traceparent`) |
| `redis` | JSON payload field (`traceparent`) — old workers that predate this change see an unknown field and ignore it |
| `postgres` | Columns `traceparent` / `tracestate` on `autumn_jobs` — add them via the bundled migration `20260519000000_add_trace_context_to_jobs` |

The `job.execute` span carries the `otel.kind = consumer` attribute so APM
tools render it as a messaging consumer span.

`#[mailer]` methods that call `deliver_later` inherit the current tracing span
for the spawned delivery task, so log records emitted during background
delivery remain correlated with the request that triggered them.

**Migration step for Postgres jobs.** If your app uses
`jobs.backend = "postgres"` and you upgrade to a version that includes this
change, run the migration before deploying new workers:

```shell
autumn migrate
```

or apply it manually:

```sql
ALTER TABLE autumn_jobs ADD COLUMN IF NOT EXISTS traceparent TEXT;
ALTER TABLE autumn_jobs ADD COLUMN IF NOT EXISTS tracestate  TEXT;
```

Workers compiled **without** `telemetry-otlp` skip the columns in their
`SELECT` and `INSERT` statements and are unaffected.

## Harvest Backends

| Backend | When to pick it |
|---|---|
| `local` | Local dev and single-process demos; jobs are lost on restart |
| `postgres` | Production with Postgres already in the stack; no Redis required |
| `redis` | Very high job throughput, or Redis is already a dependency |

`postgres` reuses the configured `[database]` pool and claims jobs via
`SELECT … FOR UPDATE SKIP LOCKED`, making it safe across any number of replicas
without adding Redis. Enable it with `jobs.backend = "postgres"` and run
`autumn migrate` before the first worker starts.

`redis` offers a higher throughput ceiling and sub-millisecond poll latency but
adds Redis as an infrastructure dependency. Prefer `postgres` if your ops budget
does not include Redis, and switch to `redis` if job throughput saturates the
Postgres connection pool.

## File Uploads

The `Multipart` extractor's `save_to(path)` primitive writes to the local
disk of whichever pod handled the request — invisible to the next replica
and gone on the next deploy. For multi-replica deployments that accept
user-uploaded files (avatars, attachments, generated reports), enable the
`storage` feature and pick the `S3` backend:

```toml
[storage]
backend = "s3"

[storage.s3]
bucket = "my-app-uploads"
region = "us-east-1"
```

In `prod`, `backend = "local"` fails fast at startup unless you set
`storage.allow_local_in_production = true` — same pattern as the session
backend's memory-in-prod check.

See [storage.md](storage.md) for the full backend, configuration, and
production-checklist story.

## Sessions

In-memory sessions are fine for local development and single-process demos.
They are the wrong default for horizontally scaled apps.

Use:

```toml
[session]
backend = "redis"

[session.redis]
url = "redis://redis:6379"
key_prefix = "my-app:sessions"
```

The `prod` profile warns when you keep `backend = "memory"` without explicitly
acknowledging it via `session.allow_memory_in_production = true`.

A managed Redis that only exposes a TLS port takes a `rediss://` URL here —
see [Redis over TLS](#redis-over-tls-rediss).

## Mail

The `mail` cargo feature gives apps a `Mailer` extractor and log/file/SMTP
transports. Development defaults to `transport = "log"` so password-reset and
signup flows can be built before SMTP exists. Production rejects log transport
unless `mail.allow_log_in_production = true` is set.

Use SMTP in production:

```toml
[mail]
transport = "smtp"
from = "Acme <noreply@example.com>"

[mail.smtp]
host = "smtp.example.com"
port = 587
username = "apikey"
password_env = "SMTP_PASSWORD"
tls = "starttls"
```

For durable retries across replicas, register a durable
[`MailDeliveryQueue`](mail.md#deferred-delivery-deliver_later) via
`AppBuilder::with_mail_delivery_queue` before `.run()` (see the Mail Guide
for the trait definition and an outbox example). Without one,
`try_deliver_later`/`try_deliver_later_eager` return
`MailError::NoDurableQueueInProduction` in `prod` unless you explicitly set
`mail.allow_in_process_deliver_later_in_production = true`, which
acknowledges the in-process Tokio fallback (the plain `deliver_later`/
`deliver_later_eager` methods only log that error, so a caller using them
can't observe it). The fallback is fine for local development and small
single-process deployments but is not durable across restarts or replicas.
Apps that only call `mailer.send(...)` never reach this guard and need
neither a queue nor the flag.

When email dispatch is coordinated with DB writes, use
[`Db::tx`](transactions.md) for the database side so the write set commits or
rolls back atomically. Call `deliver_later` inside the transaction closure and
Autumn will automatically defer the mail spawn until after commit, so no emails
will be sent for rolled-back writes. That deferral is still process-local; use a
durable outbox or queue row written inside the transaction when the handoff must
survive restarts. See
[Transactions -> after_commit](transactions.md#after_commit--post-commit-process-local-callbacks)
for the complete pattern including jobs.

## Background Work

Use `#[scheduled]` when:

- the task is small and in-process
- duplicate execution per replica is acceptable or explicitly coordinated
- you do not need durable retries or workflow history

Use Harvest when:

- the task must survive restarts
- retries and visibility matter
- work should be coordinated across replicas
- you are really describing a workflow, not a cron callback

## Split web/worker topology

The same image can run as two deployments: **web** replicas that serve HTTP and
enqueue jobs, and **worker** replicas that drain jobs and run the scheduler.
Pick the role per deployment with `AUTUMN_ROLE` — no app code or image changes.
This is the standard "web tier + separately scaled worker tier" production shape
(Rails/Sidekiq, Laravel Horizon, Django/Celery, Loco.rs `--worker`); scale,
deploy, and drain each tier on its own.

Requirements:

- **A durable jobs backend** — `jobs.backend = "postgres"` or `"redis"`. The
  `local` backend is in-process, so a web replica would enqueue where no worker
  can drain. Autumn rejects a split role on `local` at startup and
  `autumn doctor --strict` flags it. See
  [Web and worker process roles](jobs.md#web-and-worker-process-roles).
- **The same migration gate** — both tiers share one backend, so run the
  dedicated migration job (below) once before either tier starts.
- **Probes on the worker tier** — a `worker` replica serves no user routes but
  still binds `/live`, `/ready`, `/startup`, and `/actuator/*`, so wire the same
  liveness/readiness probes you use for web.

Docker Compose sketch — one image, two roles, shared Postgres backend:

```yaml
services:
  # db + migrate: as in the Migration Jobs section below — migrate runs once, first.

  web:
    image: my-app:latest
    environment:
      AUTUMN_ROLE: web
      AUTUMN_DATABASE__PRIMARY_URL: postgres://app:secret@db:5432/app
    depends_on:
      migrate:
        condition: service_completed_successfully

  worker:
    image: my-app:latest
    environment:
      AUTUMN_ROLE: worker
      AUTUMN_DATABASE__PRIMARY_URL: postgres://app:secret@db:5432/app
    depends_on:
      migrate:
        condition: service_completed_successfully
```

On Kubernetes this is two `Deployment`s off the same image — one with
`AUTUMN_ROLE=web`, one with `AUTUMN_ROLE=worker` — each with its own replica
count and autoscaler, both pointing the readiness probe at `/ready` so the
[rolling-deploy drain](#rolling-deploy-lifecycle) supervises worker rollouts the
same way it does web.

### Gating app-owned background work

`AUTUMN_ROLE` gates the **framework's** `#[job]`/`#[scheduled]` workers, but not
a background loop your app spawns itself in an `on_startup` hook — that runs on
every replica, including the web tier, unless you gate it. The resolved role is
exposed on `AppState` as the same value the framework used to gate its own
workers, so self-gate app-owned loops instead of re-reading `AUTUMN_ROLE`:

```rust
use autumn_web::{AppState, ProcessRole};

// on_startup hook: only start the embedded loop where workers run.
if state.role().runs_workers() {
    tokio::spawn(my_embedded_worker(state.clone()));
}
```

`state.role()` returns a `ProcessRole` with `serves_http()` / `runs_workers()`
predicates; see
[Self-gating app-owned background work](jobs.md#self-gating-app-owned-background-work).
Custom/named roles are unsupported today — for finer placement use per-queue
worker pinning (#1623) plus app-level `state.role()` gating.

## Migration Jobs

For multi-replica deployments, do not rely on each web process racing to apply
migrations. Run migrations once as a dedicated job, then start the web
deployment after it succeeds.

The migration job must target the primary/write role:

```bash
AUTUMN_DATABASE__PRIMARY_URL="postgres://user:pass@primary:5432/app" autumn migrate
```

`DATABASE_URL` still works for single-primary apps, but naming
`AUTUMN_DATABASE__PRIMARY_URL` keeps the deployment contract explicit. Keep
`auto_migrate_in_production = false` on web replicas unless you are deliberately
running a single-process deployment.

### Waiting for the Database at Cold Start

On a fresh Compose stack, a cold managed Postgres, or a Kubernetes pod that
races ahead of its database, `autumn migrate` may be called before the database
is accepting connections. Instead of crashing and requiring a bespoke
`wait-for-it.sh` wrapper, you can tell `autumn migrate` to wait:

```bash
# Via environment variable (recommended for container deployments):
AUTUMN_DATABASE__STARTUP_WAIT_SECS=60 autumn migrate

# Via CLI flag (overrides the environment variable and config file):
autumn migrate --wait 60

# Via autumn.toml:
# [database]
# startup_wait_secs = 60
```

When `startup_wait_secs` is set to a non-zero value, `autumn migrate` retries
the initial connection with capped exponential backoff (starting at 500 ms,
doubling each attempt, capped at 5 s) until the database responds or the total
wait exceeds the configured limit. On each retry, the attempt number and delay
are printed so you can see progress in container logs.

Only transient "server not yet reachable" errors are retried (connection
refused, network unreachable, database system starting up, etc.). Authentication
failures, missing databases, and malformed URLs fail immediately so you do not
burn the entire wait window on a configuration mistake.

The default is `0`, which preserves today's fail-fast behavior with no
behavioral change for existing deployments.

**Docker Compose example** — no `depends_on: condition: service_healthy` or
`wait-for-it.sh` required:

```yaml
services:
  db:
    image: postgres:16
    environment:
      POSTGRES_USER: app
      POSTGRES_PASSWORD: secret
      POSTGRES_DB: app

  migrate:
    image: my-app:latest
    command: autumn migrate
    environment:
      AUTUMN_DATABASE__PRIMARY_URL: postgres://app:secret@db:5432/app
      AUTUMN_DATABASE__STARTUP_WAIT_SECS: "60"
    depends_on:
      - db

  web:
    image: my-app:latest
    environment:
      AUTUMN_DATABASE__PRIMARY_URL: postgres://app:secret@db:5432/app
    depends_on:
      migrate:
        condition: service_completed_successfully
```

## Migration Safety Preflight

Before applying migrations in production, run the safety check in CI or locally:

```bash
autumn migrate check
```

`autumn migrate check` reads every `migrations/*/up.sql` file from disk (no
database connection required) and classifies each SQL statement by its risk for
a rolling deploy. It exits **0** when all `up.sql` statements are fully safe and
**1** when any is `potentially-blocking`, `destructive`, `irreversible`,
`data-backfill`, `manual-review`, or `unsupported`. `down.sql` is classified and
reported too, but does not decide the exit code: it runs on `autumn migrate
down`, not on deploy. Each finding includes a one-line reason and a concrete
next action.

Classification follows the app's own backend (#1906). The example below is a
Postgres app; on SQLite the same command applies SQLite's rules — see
[SQLite in production](./sqlite-in-production.md#migration-mechanics-on-sqlite).

Example output:

```
✓ 20240301_create_posts                 safe
✗ 20240312_add_index_posts_title        potentially-blocking
  └─ CREATE INDEX (non-concurrent): holds an exclusive table lock for the entire build
     next: Use CREATE INDEX CONCURRENTLY instead.
✗ 20240315_rename_body_to_content       irreversible
  └─ RENAME COLUMN: breaks queries from old replicas still referencing the old name
     next: Use expand/contract: add the new column, dual-write, backfill, update code, drop old.
```

### Risk levels

| Level | Meaning |
|---|---|
| `safe` | Additive, backward-compatible. Safe for rolling deploys. |
| `potentially-blocking` | May acquire a table-level lock on large datasets. |
| `destructive` | Removes data or structure; old replicas may fail until restart. |
| `irreversible` | Cannot be undone without a multi-step expand/contract cycle. |
| `data-backfill` | Schema change is safe but requires a separate backfill job. |
| `manual-review` | Autumn cannot auto-classify this statement; operator review required. |
| `unsupported` | The backend has no syntax for this statement; it fails at apply time. SQLite only (#1906). |

### Adding `autumn migrate check` to CI

```yaml
# GitHub Actions example
- name: Check migration safety
  run: autumn migrate check
```

Place this step after building the binary and before any deployment step that
applies migrations. A non-zero exit code fails the pipeline, preventing unsafe
migrations from reaching production unreviewed.

### The expand/contract pattern

Most "dangerous" schema changes can be made safe by splitting them into two
separately deployed changes:

1. **Expand** — add the new column or table alongside the old one; update code
   to dual-write and read from either. This migration is `safe` and can be
   applied with a rolling deploy.
2. **Contract** — once all replicas run the new code, remove the old
   column/table. This migration may be `destructive` but is now safe because no
   running code references the old structure.

Common patterns:

| Goal | Naïve (unsafe) | Expand/contract (safe) |
|---|---|---|
| Rename a column | `RENAME COLUMN old TO new` | Add `new`, dual-write, backfill, drop `old` |
| Change a column type | `ALTER COLUMN x TYPE bigint` | Add `x_new bigint`, migrate data, swap code, drop `x` |
| Drop a column | `DROP COLUMN body` | First deploy: remove all reads/writes; second deploy: `DROP COLUMN` |
| Add NOT NULL | `ADD COLUMN x INT NOT NULL` | Add nullable, backfill, add constraint `NOT VALID`, validate |

### When a maintenance window is required

Some operations cannot be made zero-downtime regardless of the pattern:

- **Non-concurrent index creation** on very large tables (prefer
  `CREATE INDEX CONCURRENTLY`).
- **Adding a primary key** to a table with existing rows without `NOT VALID`.
- **Enabling row-level security** on a table referenced by live queries.

For these, schedule a maintenance window and communicate the outage to users.

## Database Topology

Use the `[database]` section to declare the shape:

```toml
[database]
primary_url = "postgres://user:pass@primary:5432/app"
replica_url = "postgres://user:pass@replica:5432/app" # optional
primary_pool_size = 10
replica_pool_size = 5
replica_fallback = "fail_readiness" # or "primary"
auto_migrate_in_production = false
```

`Db`, transactions, advisory locks, scheduled-task coordination, and migrations
use the primary role. Read-oriented code may use the replica pool when one is
configured; if the replica is missing or stale, choose one deterministic
behavior: fail readiness (`fail_readiness`) or explicitly fall back to the
primary (`primary`).

`autumn doctor --strict` checks the topology contract: missing primary role,
unreachable primary/replica endpoints, unsafe production migration ownership,
and stale replica migration versions. Diagnostics name the failing role and
redact credentials.

The distributed bookmarks example uses this shape explicitly with a primary,
streaming replica, one migration job, two web replicas, and a readiness gate
that fails while the replica has not replayed the latest Diesel migration.

## Replication lag and read-your-own-writes

> **Warning — the classic anomaly.** Replication is asynchronous. A read
> immediately after a write can land on a lagging replica and return stale data
> — the user submits a form, is redirected, reloads the page, and the change
> appears gone. DDIA §5 calls this the *read-your-own-writes* anomaly.

Autumn's default behavior routes all replica-eligible reads to the replica
regardless of whether the same request performed a write. Add
`read_your_writes` in `[database]` to pin post-write reads to the primary:

```toml
[database]
primary_url   = "postgres://user:pass@primary:5432/app"
replica_url   = "postgres://user:pass@replica:5432/app"

# Option A — intra-request pin only (Laravel "sticky")
read_your_writes = "request"

# Option B — cross-request pin via signed cookie (Rails automatic role switching)
read_your_writes = "session"
pin_after_write_secs = 5          # how long the cookie pins reads; default 5 s
```

### Modes

| Mode | What happens | Tradeoff |
|------|-------------|----------|
| `off` (default) | No pinning. Replica reads always use the replica. | Zero overhead; stale reads possible after writes. |
| `request` | Once the current request checks out a **primary** connection (via `Db` or a generated mutating method), all subsequent replica-eligible reads in that request route to the primary. | Eliminates intra-request anomalies at negligible overhead. A read-only handler that still injects `Db` will pin its reads unnecessarily — document this in your team's conventions. |
| `session` | Like `request`, plus a signed `autumn.ryw` cookie pins the same client's reads to the primary for `pin_after_write_secs` seconds after a write. | Eliminates post-redirect anomalies. Adds a small cookie round-trip and increases primary read load during the pin window. |

### Composing with `replica_fallback`

`read_your_writes` and `replica_fallback` are independent: you can have
`read_your_writes = "request"` and `replica_fallback = "primary"` simultaneously.
When the replica is unready and fallback is `primary`, all reads already go to
the primary regardless of the pin. When fallback is `fail_readiness`, a
`ReadRoute::Unavailable` repo returns an error on reads even while pinned — the
pin does not bypass the health gate.

### Observability

Every pin-redirected read (a replica-eligible read sent to the primary because
the pin is active) increments `autumn_read_your_writes_pins_total` in
`/actuator/metrics` and emits a `DEBUG` event to the `autumn::db` tracing
target. A spike in this counter after a deployment indicates more reads than
expected are being pinned — tune `pin_after_write_secs` or audit which handlers
are injecting `Db` unnecessarily.

## Shared Cache

In-process Moka caches are the zero-config default and are perfect for
local development. Each replica holds its own independent store, so:

- `#[cached]` may return stale data depending on which replica answers.
- `CacheResponseLayer` invalidations on one pod are invisible to peers.

For multi-replica production deployments, enable the Redis backend via
`autumn-cache-redis`:

```toml
# autumn.toml
[cache]
backend = "redis"

[cache.redis]
url = "redis://redis:6379"
key_prefix = "myapp:cache"
```

```rust
// main.rs
use autumn_cache_redis::RedisCachePlugin;

autumn_web::app()
    .plugin(RedisCachePlugin::new())
    .routes(routes![...])
    .run()
    .await;
```

`autumn-cache-redis` requires the `autumn-cache-redis` crate:

```toml
# Cargo.toml
[dependencies]
autumn-cache-redis = "0.7"
```

`CacheResponseLayer::from_app(&state)` returns `Some(layer)` wired to the
configured Redis backend when one has been registered, or `None` when running
with the default per-function Moka caches.

The `memory` default produces a startup warning in the `prod` profile — the
same pattern as sessions and file storage.

For read-through fills that coalesce concurrent misses into a single
recompute (in-process single-flight, plus an opt-in distributed fill lock and
stale-while-revalidate on the Redis backend), see [Cache Stampede
Protection](cache-stampede.md).

## Redis over TLS (`rediss://`)

Managed Redis usually speaks TLS and often speaks *only* TLS. Azure Cache for
Redis provisioned by `autumn release init --target azure-container-apps` sets
`non_ssl_port_enabled = false`, and ElastiCache with transit encryption on
behaves the same way, so the only URL you get is a `rediss://` one.

Every Redis-backed subsystem accepts it — sessions, channels, jobs, job
tracking, idempotency, webhook replay, rate limiting, and the
`autumn-cache-redis` cache:

```toml
[session.redis]
url = "rediss://:<access-key>@my-cache.redis.cache.windows.net:6380"
```

Valkey's `valkeys://` scheme works the same way. No extra Cargo feature is
needed; TLS support is compiled in.

Certificates are validated against the public CA store. A managed Redis that
presents a private or provider-internal CA — Google Memorystore, for instance
— will not validate, which is why `autumn release init --target
gcp-cloud-run` provisions Memorystore with `transit_encryption_mode =
"DISABLED"` and keeps the connection inside the VPC instead.

Autumn installs the process-wide rustls `CryptoProvider` (`ring`) that the
`redis` crate's TLS transport requires, once and only for TLS URLs. If your
application installs its own provider — for example `aws-lc-rs` — install it
at the top of `main`, before the app builder runs: Autumn keeps whatever is
already installed, and it may reach for one as early as config validation. A
plaintext `redis://` URL never claims the default, so it cannot pre-empt that
choice.

Redis URLs carry credentials, so Autumn redacts the password from any URL it
logs. Do the same in your own code — `autumn_web::redis_tls::redact_url`.

## Concurrent Writes

### The lost-update problem

With more than one replica, two requests can read the same row, compute
independent changes, and both write back — the second write silently overwrites
the first. No error is raised, no conflict is detected, data is lost.

### Optimistic concurrency via `#[lock_version]`

Add the attribute to any model field named `lock_version`:

> **Generated apps get this for free.** Declaring a `lock_version` column
> in `autumn generate model` / `autumn generate scaffold` emits the
> attribute (and the column's `DEFAULT 0`) for you, and a scaffold
> additionally wires the whole conflict-aware HTML edit flow — hidden
> version field, guarded `UPDATE`, and a 409 re-render instead of a lost
> update. See
> [Concurrent edits: `lock_version`](./generators.md#concurrent-edits-lock_version-optimistic-locking).

```rust
#[autumn_web::model]
pub struct Article {
    #[id]
    pub id: i64,
    pub title: String,
    pub body: String,
    #[lock_version]
    pub lock_version: i32,
    #[default]
    pub created_at: chrono::NaiveDateTime,
    #[default]
    pub updated_at: chrono::NaiveDateTime,
}
```

The framework then:

1. Stores the current `lock_version` as a counter column in the database.
2. Requires the client to send the expected version alongside its update
   payload (the generated `UpdateArticle` struct carries this automatically).
3. On write, issues an atomic `UPDATE … WHERE id = $1 AND lock_version = $2`
   and increments the counter only if the row matched.

If the row was updated between the client's read and its write — i.e. the
stored version is no longer what the client expected — the UPDATE matches zero
rows and the repository returns `RepositoryError::Conflict`, which the
framework maps to HTTP 409 with an RFC 7807 problem body:

```json
{"type": "about:blank", "status": 409, "title": "Conflict", "detail": "..."}
```

For htmx clients, the framework also emits the response header:

```
HX-Trigger: {"autumn:conflict":true}
```

Your client-side script can listen for that event and re-fetch the current
version before letting the user resubmit.

A handler that catches the conflict and signals a retry:

```rust
async fn update_article(
    State(repo): State<ArticleRepository>,
    Path(id): Path<i64>,
    Form(input): Form<UpdateArticle>,
) -> Response {
    match repo.update(id, input).await {
        Ok(article) => Redirect::to(&format!("/articles/{}", article.id)).into_response(),
        Err(RepositoryError::Conflict { .. }) => {
            // Re-fetch the current version and tell the user to retry
            let current = repo.find(id).await.unwrap();
            (StatusCode::CONFLICT, EditTemplate { article: current, conflict: true })
                .into_response()
        }
        Err(e) => e.into_response(),
    }
}
```

Optimistic concurrency is the right default for most web applications: reads
are cheap, conflicts are rare, and throughput scales well.

### Pessimistic concurrency via `with_lock`

For low-contention but high-consequence writes — think inventory deductions,
financial ledger entries, or seat reservations — you cannot afford to retry
after detecting a conflict because another request may have already acted on
the same data. Use `with_lock` to acquire a database-level advisory or
row-level lock before reading:

```rust
repo.with_lock(id, |row, conn| async move {
    // `row` is the freshly locked Page; `conn` is the transaction connection.
    // Any writes here are serialized against other `with_lock` callers for
    // the same `id`.
    row.seats_remaining -= 1;
    diesel::update(seats::table.find(row.id))
        .set(seats::seats_remaining.eq(row.seats_remaining))
        .execute(conn)
        .await?;
    Ok(row)
}.scope_boxed()).await
```

The closure runs inside a transaction. The lock is released when the
transaction commits or rolls back.

### Trade-offs

| | Optimistic | Pessimistic |
|---|---|---|
| **Throughput** | High — no blocking between readers | Lower — concurrent writers queue |
| **Latency** | Low on the happy path; a retry adds one round trip | Consistently higher; each writer waits for the lock |
| **Complexity** | Low — framework handles version checks | Moderate — closure-based API, must reason about deadlocks |
| **Best for** | Typical CRUD, forms, wiki edits, profile updates | Inventory, ledger, seat/slot reservation, anything where retry is unsafe |

### Cache invalidation after writes

`after_update` hooks receive a `MutationContext` that accepts
`ctx.invalidate("key")` calls to declare cache keys that should be evicted
after the write commits. This is coordinated with the shared-cache integration
(#535) so that the correct backend — Moka or Redis — is targeted regardless of
which replica processed the write:

```rust
async fn after_update(&self, ctx: &mut MutationContext, page: &Page) -> AutumnResult<()> {
    ctx.invalidate(format!("pages:{}", page.id));
    ctx.invalidate("pages:all");
    Ok(())
}
```

## Rolling Deploy Lifecycle

Autumn implements a documented, tested shutdown sequence that gives
load balancers and orchestrators time to drain traffic before the replica
exits. Every phase has a defined ordering guarantee and a configurable
timeout.

### Shutdown phases

| # | Phase | Description |
|---|-------|-------------|
| 1 | **signal_received** | SIGTERM or Ctrl-C arrives; Autumn begins the shutdown sequence and logs a structured `phase=signal_received` event with configured timeouts. |
| 2 | **ready_draining** | `/ready` flips to `503 Service Unavailable` **strictly before** the TCP listener closes. Upstream load balancers can now deregister the replica. |
| 3 | **prestop_grace** | Autumn sleeps `server.prestop_grace_secs` (default `5`). Set this to at least your LB's health-check interval plus deregistration propagation time. |
| 4 | **ws_closing** | The WebSocket shutdown token fires. Handlers that opt into `WithShutdown` should send a `1001 Going Away` close frame so clients can reconnect to another replica. Handlers that do not use `WithShutdown` will have their connections closed without a close frame. |
| 5 | **listener_stopping** | The TCP listener stops accepting new connections. `#[job]` workers and `#[scheduled]` tasks stop dequeuing/launching new work — they share the same cancellation token as the listener. |
| 6 | **in_flight_drain** | In-flight HTTP requests complete for up to `server.shutdown_timeout_secs` (default `30`). Requests still running at the deadline are aborted and counted in `autumn_shutdown_aborted_requests_total`. The process exits with code `1` and a structured log line naming the exceeded phase. |
| 7 | **app_hooks** | `on_shutdown` hooks run in **LIFO registration order** with a per-hook and total budget equal to `shutdown_timeout_secs`. Plugin hooks registered during `build()` run after app hooks (LIFO means last-registered runs first). Overruns are logged at WARN but do not block the remaining budget. |
| 8 | **telemetry_flush** | OpenTelemetry span exporter flushes buffered spans (handled by the `_telemetry_guard` drop). |
| 9 | **db_pool_close** | The Diesel connection pool is dropped with the process. |
| 10 | **exit 0** | Process exits with code `0` when all phases complete inside their deadlines. |

### Configuration

```toml
[server]
# Seconds /ready returns 503 before the listener closes (default: 5).
# Tune to: LB health-check interval + deregistration propagation time.
prestop_grace_secs = 5

# Maximum seconds for in-flight requests to drain (default: 30).
shutdown_timeout_secs = 30
```

Environment variable overrides:

```
AUTUMN_SERVER__PRESTOP_GRACE_SECS=10
AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS=60
```

### Kubernetes / ECS configuration

Wire `prestop_grace_secs` to your `preStop` hook and termination grace period:

```yaml
# Kubernetes Deployment example
spec:
  template:
    spec:
      # Formula: preStop_hook_secs + prestop_grace_secs + shutdown_timeout_secs + buffer
      # shutdown_timeout_secs covers drain AND on_shutdown hooks combined
      # (they share one budget, not two separate windows).
      # If you use a Kubernetes preStop hook, include its duration in the total.
      # Example: 5 (preStop sleep) + 5 (prestop_grace) + 30 (drain+hooks) + 10 (buffer) = 50 s.
      terminationGracePeriodSeconds: 50
      containers:
        - name: app
          readinessProbe:
            httpGet:
              path: /ready
              port: 3000
            failureThreshold: 1          # deregister immediately on first 503
          lifecycle:
            preStop:
              exec:
                # Optional: give the LB extra time before SIGTERM arrives.
                command: ["sleep", "5"]
```

> The `autumn_shutdown_aborted_requests_total` metric is available at
> `/actuator/metrics` under `http.shutdown_aborted_requests_total`. Alert
> when this counter is non-zero across rolling deploys — it indicates that
> `shutdown_timeout_secs` is too short for your workload.
>
> **Note:** when drain times out the process exits with code `1` immediately
> after recording the count. The in-memory counter is lost at that point.
> The structured log line (`phase=in_flight_drain autumn_shutdown_aborted_requests_total=N`)
> emitted just before `exit(1)` is the durable signal — ship those logs to
> your log aggregator and alert on `exit_code=1` log events as a backup SLI.

### Job and scheduler drain contract

`#[job]` workers stop dequeuing new jobs when the listener closes (phase 5).
Running job handlers continue concurrently during HTTP drain and are given a
best-effort opportunity to finish — but because their `tokio::spawn` handles
are not retained, the process does not wait for them after HTTP drain
completes. Handlers that cannot finish quickly should use the crash-safe
checkpoint path (see [Jobs guide](jobs.md)) so work can be resumed on the
next replica. The same applies to `#[scheduled]` tasks: the scheduler stops
launching new runs at phase 5, and any in-progress run proceeds
concurrently during drain but is not awaited at shutdown.

### WebSocket drain contract

Every `#[ws]` handler that uses `WithShutdown` receives a `CancellationToken`
that is cancelled at phase 4. Handlers should send a close frame on
cancellation:

```rust
#[ws("/chat")]
async fn chat() -> impl WsHandler {
    WithShutdown(
        |mut socket: WebSocket, shutdown: CancellationToken| async move {
            loop {
                tokio::select! {
                    msg = socket.recv() => { /* handle */ }
                    () = shutdown.cancelled() => {
                        socket.send(Message::Close(Some(CloseFrame {
                            code: CloseCode::Away,
                            reason: "server restarting".into(),
                        }))).await.ok();
                        break;
                    }
                }
            }
        },
    )
}
```

### on_shutdown hook ordering

Hooks run in **LIFO** (last-in, first-out) order — the last hook registered
runs first. This means infrastructure registered early in `main()` shuts down
last, after the code that depends on it.

Plugin ordering rule: hooks run in registration order, LIFO. Plugins call
their hooks during `build()`, which runs before app `.on_shutdown()` calls
in `main()` — so app hooks typically run before plugin hooks. However, any
`.on_shutdown()` call after a `.plugin()` call will run before that plugin's
hook.

```rust
autumn_web::app()
    .plugin(MyPlugin)          // plugin hook registered first → runs last
    .on_shutdown(|| async {    // app hook registered second → runs first
        tracing::info!("app cleanup");
    })
    .run()
    .await;
```

## Minimal Deployment Checklist

Before calling an Autumn app "cloud ready", verify:

- probes target `/live`, `/ready`, and `/startup`
- logs or traces land in your collector
- sessions are externalized if replicas > 1
- cache uses the Redis backend if replicas > 1
- file uploads use the `S3` blob store if replicas > 1
- mail uses SMTP, not log/file transport
- `autumn migrate check` passes in CI before the deploy job
- migrations run before web rollout via a dedicated migration job
- destructive/irreversible migrations follow the expand/contract pattern
- background jobs use the right runtime model
- a split web/worker topology (`AUTUMN_ROLE=web` / `worker`) runs on a durable jobs backend (`postgres`/`redis`, never `local`), with worker replicas exposing `/live` + `/ready`
- `autumn_jobs` has `traceparent` / `tracestate` columns if using the Postgres backend with `telemetry-otlp`
- multi-replica write paths use `#[lock_version]` (optimistic) or `with_lock` (pessimistic) to prevent lost updates
- the generated container image builds without manual template surgery
- `server.prestop_grace_secs` is tuned to match your load balancer's deregistration propagation time
- `terminationGracePeriodSeconds` (Kubernetes) or equivalent is set to `preStop_hook_secs + prestop_grace_secs + shutdown_timeout_secs + buffer` (`shutdown_timeout_secs` covers drain **and** hooks combined)
- `autumn_shutdown_aborted_requests_total` is monitored and alerts on any non-zero value after a rolling deploy
