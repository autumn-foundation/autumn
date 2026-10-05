# Multi-Replica Scheduled Tasks

`#[scheduled]` defaults to the original in-process behavior: every running
replica owns its own timer. That is convenient in development and preserves
the local-compatible behavior of earlier releases, but it is not safe for tasks
that send emails, call paid APIs, expire tokens, charge cards, or mutate shared
state.

For multi-replica deployments, configure the scheduler backend to `postgres`.
Autumn then derives a global tick key for each scheduled task invocation and
records the tick in a Postgres table through the existing `Db` pool. Only the
replica that inserts the tick row runs that tick.

If a replica boots with `backend = "in_process"` and a hint shows more than one
replica, Autumn logs a warning. The hints are: `jobs.backend` is `postgres` or
`redis`, `AUTUMN_REPLICAS` is more than 1, or `KUBERNETES_SERVICE_HOST` is set.

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

### The tick table

Each fleet tick is one row in `autumn_scheduler_ticks`:

```sql
INSERT INTO autumn_scheduler_ticks (key_prefix, task_name, tick_key, owner, expires_at)
VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5))
ON CONFLICT DO NOTHING
RETURNING generation
```

Only the replica that gets a `generation` back runs the tick. The row stays
after the run. Thus, a replica whose timer reaches the same tick later (timer
skew, a GC pause, a slow boot) does not run it again.

- A row stays for `lease_ttl_secs`. A fixed-delay row stays for its delay plus
  `lease_ttl_secs`, because each replica starts its timer at its own boot.
  Then the next claim deletes the row. Set `lease_ttl_secs` longer than the
  spread between the replicas' clocks.
- The claim and the prune use the database clock (`now()`), not the replica
  clocks.
- The coordinator keeps no session state and holds no connection while a tick
  runs. Thus, it works behind a transaction-mode PgBouncer.

The runtime creates the table on first use. If the database role cannot run
`CREATE TABLE`, apply this DDL (`autumn_web::scheduler::PG_TICK_TABLE_DDL`)
before you deploy, and grant `SELECT`, `INSERT`, `DELETE` on the table and
`USAGE` on its sequence:

```sql
CREATE TABLE IF NOT EXISTS autumn_scheduler_ticks (
    key_prefix TEXT        NOT NULL,
    task_name  TEXT        NOT NULL,
    tick_key   TEXT        NOT NULL,
    owner      TEXT        NOT NULL,
    generation BIGSERIAL   NOT NULL,
    claimed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (key_prefix, task_name, tick_key)
);
CREATE INDEX IF NOT EXISTS idx_autumn_scheduler_ticks_expires_at
    ON autumn_scheduler_ticks (expires_at);
```

### Fencing token

`generation` comes from a sequence, so it increases with each claim. A task
can read it as a fencing token:

```rust
#[scheduled(cron = "0 0 0 * * *", name = "nightly-invoice")]
async fn nightly_invoice(_state: AppState) -> AutumnResult<()> {
    let _token = autumn_web::scheduler::current_tick().and_then(|tick| tick.fencing_token());
    // Write the token with each side effect. Refuse a write with an older token.
    Ok(())
}
```

`current_tick()` returns `None` outside a scheduled task, and in a task that
the handler spawns. `fencing_token()` is `None` on the `in_process` and
`sqlite` backends, and for `per_replica` tasks.

## Configure SQLite Coordination

SQLite has no shared server, and a SQLite deployment is single-host, so
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
longest a tick body can take. The Postgres coordinator keeps its tick row in
the same way.

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

The policy is **at-most-once per tick**.

- **The leader finishes.** The tick row stays, so no other replica runs that
  tick.
- **The leader crashes mid-tick.** The tick row stays, so no replica runs that
  tick again. The next tick runs as usual. Autumn does not retry a lost tick.
- **The tick runs too long.** `lease_ttl_secs` also limits one scheduled
  invocation. Autumn stops it, records it as failed, and does not run it again.
- **A stuck tick.** Tick keys include the schedule bucket, so a stuck older
  tick does not block the next tick.

This is not distributed exactly-once delivery. Under partitions, clock skew, or
hard restarts near a boundary, design scheduled tasks to be idempotent. Use the
fencing token to reject a stale write. If the workflow needs durable retries,
history, and stronger orchestration semantics, use Autumn Harvest instead of
`#[scheduled]`.
