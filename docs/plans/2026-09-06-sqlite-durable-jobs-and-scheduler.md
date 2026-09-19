# SQLite durable jobs and single-host scheduler (#1907)

Status: implemented.

## Problem

The durable jobs subsystem and the scheduler coordinate with Postgres advisory
locks. SQLite has no analog. A SQLite deployment is single-host, so it needs a
single-host strategy, not a port of the advisory-lock approach.

Before this change, SQLite had only:

- `jobs.backend = "local"` — an in-process queue. A restart loses queued work.
- `scheduler.backend = "in_process"` — every process fires every tick. Two
  processes on one host both fire.

`docs/guide/sqlite-in-production.md` promises more: a job queue that is a table
in the same SQLite file, claimed with a single-writer claim, and restart-safe.

## Options considered

1. **Table-backed durable queue** (`jobs.backend = "sqlite"`) plus a
   **table-backed scheduler lease** (`scheduler.backend = "sqlite"`). Chosen.
2. Make the Postgres backend backend-generic and reuse it. Rejected: it
   rewrites the most critical subsystem in the framework and puts the Postgres
   hot path at risk for a SQLite-only gain.
3. A write-ahead journal file next to the in-process queue. Rejected: it
   reinvents a queue with no SQL surface for the admin dashboard.
4. Require Redis for durability. Rejected: the SQLite tier exists to remove that
   requirement.
5. An `flock`-based leader file. Rejected: file locks are unreliable on network
   filesystems, which the guide already warns against.

## Reverse brainstorm — how this could fail, and the guard

| Failure | Guard |
| --- | --- |
| Two workers run one job | The claim is one atomic `UPDATE … WHERE status='enqueued'`. Every settle carries `WHERE id=? AND claimed_by=? AND status='running'`. |
| A crash loses in-flight work | Rows persist. Stale-claim recovery re-enqueues rows whose claim is older than the visibility timeout, at boot and on an interval. |
| Long write transactions wedge the file | The claim is a single statement. No transaction spans a handler. |
| Idle workers busy-poll | Bounded poll interval, plus an in-process notify that wakes a worker on same-process enqueue, and a read probe before the write-locking claim. |
| The Postgres path regresses | Every new path is `#[cfg(feature = "sqlite")]`. Postgres code is untouched. |
| A typo silently degrades durability | `jobs.backend = "sqlite"` on a build without the feature is refused, not folded into the `local` fallback. |
| Terminal rows accumulate forever | The maintenance loop prunes expired tracked-job records, and terminal job rows once `retention.job_history` is set. |
| Timestamps break `#[sim_test]` replay | All times come from the injected clock. No `NOW()`, no `strftime`. |
| Two hosts both hold the scheduler lease | The tier is single-host by contract, and every Postgres-only primitive that implies a fleet is refused at boot. The lease coordinates processes on one host; two hosts sharing one file over a network filesystem is out of contract and is documented as such. |

## Design

### Jobs

`autumn/src/job/sqlite.rs` holds the backend. It owns table `autumn_jobs` with
the Postgres column names. Timestamps are epoch-millis integers, computed in
Rust from the injected clock. The runtime creates the table and its indexes at
start with `CREATE TABLE IF NOT EXISTS`, because framework migrations are
Postgres SQL and do not run on SQLite.

Claim (one statement, single-writer):

```sql
UPDATE autumn_jobs SET status='running', … 
WHERE id = (SELECT c.id FROM autumn_jobs c
            WHERE c.status='enqueued' AND c.run_at <= ? AND c.queue = ?
              AND (concurrency check)
            ORDER BY c.run_at LIMIT 1)
RETURNING …
```

SQLite serializes writers, so this is the analog of `FOR UPDATE SKIP LOCKED`.

Uniqueness uses the same partial unique index as Postgres and
`INSERT … ON CONFLICT DO NOTHING`.

### Scheduler

`SqliteLeaseSchedulerCoordinator` leases each `(task, tick)` in table
`autumn_scheduler_leases`. The row carries an owner and an expiry. A second
process observes `None` for the same tick. An expired lease is stealable, so a
crashed leader cannot wedge a task.

### Config

- `jobs.backend = "sqlite"`, with `[jobs.sqlite]` options.
- `SchedulerBackend::Sqlite`, reported as fleet-distributed.
- `split_role_requires_durable_backend` accepts `sqlite`, so a web/worker split
  on one host is valid.
