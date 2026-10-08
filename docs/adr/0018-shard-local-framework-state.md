# ADR 0018: Shard-Local Framework State

- Status: Accepted
- Date: 2026-10-08
- Deciders: Autumn maintainers
- Tags: sharding, jobs, fault-isolation, cells

## Context

Autumn routes tenant data to shards (`autumn/src/sharding.rs`). Framework
state stayed on one control database. That gives two problems (issue #3072):

1. **Shared failure.** The control database is one failure domain for the
   whole fleet. When it stops, the job workers of every shard stop.
2. **No atomic enqueue.** `enqueue_in_tx` writes `autumn_jobs` on the
   connection that it gets. With the table only on the control database, a
   shard transaction cannot write its data and its job together.

The state of each framework table before this ADR:

| State | Where | Note |
| --- | --- | --- |
| Outbox (`autumn_outbox`, `autumn_inbox`) | Each shard | The relay drains each shard pool. |
| Commit hooks, version history, derivations | Each shard | Shard migration sets. |
| Jobs (`autumn_jobs`) | Control | This ADR. |
| Idempotency keys | Memory or Redis | Not a database table. Keys have a tenant prefix. |
| Scheduler locks, sessions, feature flags, shard directory | Control | Fleet-wide by design. |

## Decision

1. **Opt-in.** `jobs.postgres.shard_local = true` makes jobs shard-local. The
   default stays `false`, so no current app changes.
2. **Schema from the same files.** At boot, the Postgres job runtime applies
   the `autumn_jobs` migrations to each shard primary. The SQL is
   `include_str!` of the files in `autumn/migrations/`. Each file is
   idempotent (`IF NOT EXISTS`). Thus the shard table cannot drift from the
   control table. A unit test fails when a new `autumn_jobs` migration is not
   in the list. The outbox and the lease lock use the same runtime-DDL
   pattern.
3. **A worker set for each shard.** The runtime starts `jobs.workers` worker
   loops and one maintenance loop for each shard pool. A worker claims, acks
   and retries on the pool that it claims from. Each shard has its own queue
   slots, so a backlog on one database does not hold the workers of another.
4. **Atomic enqueue.** `enqueue_in_tx(name, args, shard_conn)` inserts the row
   in the caller's shard transaction. The row commits or rolls back with the
   data.
5. **No shared breaker.** With `shard_local`, every in-transaction enqueue
   (shard or control connection) does not use the fleet-wide `job_queue`
   circuit breaker. The caller's transaction already fails when its database
   is down. A shared breaker would let a control-database outage stop shard
   enqueues.
6. **The rest stays.** Plain `enqueue` (no connection) still writes to the
   control database. Scheduler locks, sessions and flags stay on the control
   database: they are fleet-wide state, not tenant state.

## Consequences

### Positive

- A shard's data write and its job commit or roll back together.
- A control-database outage does not stop the workers of the shards.
- No new migration set, and no copy of the job SQL.

### Negative

- The job dashboard, the queue-depth gauges and the retention sweep read the
  control database only. Shard queues are not in them, and finished shard
  jobs stay until you delete them.
- Each shard gets `jobs.workers` loops and slots. Up to `jobs.workers` ×
  (shards + 1) jobs run at the same time, and the idle poll load grows with
  the shard count.
- `#[job]` concurrency limits and uniqueness keys apply in each table, not
  across shards.
- The runtime makes the shard tables in a background task after boot. An
  `enqueue_in_tx` before that fails. The app role needs `CREATE` on each
  shard. Each boot runs the idempotent DDL under a 3 s lock timeout and
  retries with backoff.
- A job that `enqueue` (not `enqueue_in_tx`) writes is still on the control
  database.
- Cross-shard transactions stay out of scope. A transaction spans one shard.

## Evidence

- `autumn/tests/integration/shard_local_jobs.rs`: a rolled-back shard
  transaction leaves no job and no data. A committed one leaves both, on the
  shard only. The shard's worker runs the job.

## Alternatives considered

- **A copy of the job migrations in a shard migration set.** Copies drift: the
  commit-hook copy already differs from the control file.
- **Always shard-local.** It adds a table and workers to every shard of every
  current app. Opt-in is safer.
- **A two-phase enqueue (control row plus shard marker).** It needs a
  coordinator, and the control database stays in the path.
- **An outbox row that the relay turns into a job.** `Outbox::enqueue_job`
  exists and stays the choice for the Redis and `SQLite` backends. On Postgres,
  a direct shard row has one step fewer.
