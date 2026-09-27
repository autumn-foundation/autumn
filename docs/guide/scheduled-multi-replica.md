# Multi-Replica Scheduled Tasks

`#[scheduled]` defaults to the original in-process behavior: every running
replica owns its own timer. That is convenient in development and preserves
the local-compatible behavior of earlier releases, but it is not safe for tasks
that send emails, call paid APIs, expire tokens, charge cards, or mutate shared
state.

For multi-replica deployments, configure the scheduler backend to `postgres`.
Autumn then derives a global tick key for each scheduled task invocation and
uses Postgres advisory locks through the existing `Db` pool. Only the replica
that acquires the lock runs that tick.

## Configure Postgres Coordination

```toml
[database]
url = "postgres://postgres:postgres@db:5432/app"

[scheduler]
backend = "postgres"
lease_ttl_secs = 300
key_prefix = "myapp:scheduler"
```

The same settings can be supplied with environment variables:

```bash
AUTUMN_SCHEDULER__BACKEND=postgres
AUTUMN_SCHEDULER__LEASE_TTL_SECS=300
AUTUMN_SCHEDULER__KEY_PREFIX=myapp:scheduler
AUTUMN_SCHEDULER__REPLICA_ID=web-1
```

`replica_id` is optional. If it is not configured, Autumn uses platform
metadata such as `FLY_MACHINE_ID` or `HOSTNAME`, then falls back to the process
id. Set it explicitly when you want stable names in `/actuator/tasks`.

## Configure SQLite Coordination

SQLite has no advisory locks, and a SQLite deployment is single-host, so
`backend = "postgres"` is refused at boot there. Use `backend = "sqlite"` when
several processes share one host — a web tier next to a worker tier, or the
overlap window of a rolling restart:

```toml
[database]
url = "sqlite:///var/lib/app/app.db"

[scheduler]
backend = "sqlite"
lease_ttl_secs = 300
key_prefix = "myapp:scheduler"
```

Each `(task, tick)` is leased in an `autumn_scheduler_leases` table in the same
database file, so exactly one process runs each tick. The runtime creates that
table itself.

The lease carries an expiry, not a session. The row is what makes the tick
claimed, and it stays for the whole of `lease_ttl_secs` whether the leader
finished or died, so a process whose timer reaches the same tick a moment later
cannot run it a second time. The next acquire reaps the row once it expires.

Set the TTL longer than both the spread between the processes' timers and the
longest a tick body can take. This is stricter than the Postgres coordinator,
which frees the tick key as soon as the leader finishes.

`backend = "sqlite"` requires the `sqlite` cargo feature, and a **file-backed**
database: an in-memory target is private to each process, so every replica would
claim the same tick and run it. That is refused at boot.

A single-process SQLite app needs none of this: keep the default
`backend = "in_process"`, where the one process is always the leader.

## Declare Scheduled Tasks

Fleet coordination is the default task mode:

```rust
use autumn_web::prelude::*;

#[scheduled(every = "10s", name = "increment-counter")]
async fn increment_counter(_state: AppState) -> AutumnResult<()> {
    // Update shared state, send one digest, charge one batch, etc.
    Ok(())
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .tasks(tasks![increment_counter])
        .run()
        .await;
}
```

Use `coordination = "per_replica"` only for work that should run on every
replica, such as warming in-memory caches:

```rust
#[scheduled(every = "1m", name = "warm-local-cache", coordination = "per_replica")]
async fn warm_local_cache(_state: AppState) -> AutumnResult<()> {
    Ok(())
}
```

## Verify With Three Replicas

With a Docker Compose file that has a `db` service and a `web` service using
the same `AUTUMN_DATABASE__PRIMARY_URL`, run three web replicas:

```bash
docker compose up --build --scale web=3
```

For a `#[scheduled(every = "10s")]` task, check the shared side effect after
one minute. You should see roughly six executions, not eighteen. A restart can
add or miss one tick because the system provides at-most-once per tick under
normal operation and best-effort recovery around process churn.

You can also inspect runtime state:

```bash
curl http://localhost:3000/actuator/tasks
```

The task entry includes the configured backend, this replica id, the last
leader, the last global tick key, and the last fired timestamp:

```json
{
  "scheduled_tasks": {
    "increment-counter": {
      "schedule": "every 10s",
      "coordination": "fleet",
      "scheduler_backend": "postgres",
      "replica_id": "web-1",
      "current_leader": "web-2",
      "last_tick": "increment-counter:170000000",
      "last_fired_at": "2026-05-05T14:00:00Z",
      "status": "idle",
      "total_runs": 6,
      "total_failures": 0
    }
  }
}
```

## Failure Semantics

Postgres advisory locks are held by the database connection used for the task
tick and are released when the task completes. If the process crashes, Postgres
releases the connection lock. `lease_ttl_secs` also bounds how long Autumn will
wait for a single scheduled invocation before recording it as failed and
releasing the lease. Tick keys include the schedule bucket, so a stuck older
tick does not block the next global tick from being claimed.

This is not distributed exactly-once delivery. Under partitions, clock skew, or
hard restarts near a boundary, design scheduled tasks to be idempotent. If the
workflow needs durable retries, history, and stronger orchestration semantics,
use Autumn Harvest instead of `#[scheduled]`.
