# ADR 0016: Renew Durable Job Claims With A Heartbeat

- Status: Accepted
- Date: 2026-10-05
- Deciders: Autumn maintainers
- Tags: jobs, distributed-systems, leases, redis, postgres, sqlite

## Context

Issue #3051. The durable job backends (Postgres, Redis, `SQLite`) recover a
`running` claim when it is older than the visibility timeout (default 30s).
Nothing renewed a claim. Thus a job that ran longer than 30s ran again, at the
same time, on a second worker. The ack guard fenced the queue row, not the
side effects. A hung handler blocked its worker and ran again every 30s.

## Decision

1. **Heartbeat per running job.** The worker spawns a task that renews the
   claim every third of the visibility timeout. Postgres and `SQLite` run
   `UPDATE … SET claimed_at = now WHERE id = $1 AND claimed_by = $me AND
   status = 'running'`. Redis runs a Lua `ZADD XX` on the processing set,
   guarded by the claim token (`claimed_by`, `claimed_at_ms`).
2. **Lost lease stops the handler.** A renewal that matches no claim cancels
   the run. The worker drops the handler future and does not settle the job.
   If no renewal succeeds for two thirds of the visibility timeout, the worker
   also stops the run, before another worker can recover the claim.
3. **Execution timeout.** `#[job(timeout = "…")]` and
   `jobs.default_timeout_ms` bound one run. Expiry is a retryable failure.
   The default is `0` (no limit), as in Oban and Sidekiq. A non-zero default
   would fail long jobs that work today.
4. **Redis server time.** The claim deadline is Redis `TIME` plus the
   visibility timeout. The stale check compares it to Redis `TIME`, inside the
   recovery script. Worker clocks do not decide staleness.
5. **No schema change.** The heartbeat reuses `claimed_at` and the existing
   processing set.

`verification/job_lease.rs` models the claim state machine in Verus. It proves
that a renewal never changes the owner, that a claim with a live heartbeat is
not recoverable, and that the give-up rule stops a worker before its claim
expires.

## Consequences

- One extra write per running job per third of the visibility timeout (one per
  10s at the default). On `SQLite` it takes the writer lock briefly.
- With no timeout, a hung handler now keeps its claim until the process stops.
  The guide tells users to set a timeout on a job that can hang.
- During a rolling deploy, an old Redis worker with a fast clock can still
  requeue a live job. The migration guide says to keep clocks in sync.

## Not done here

- Release claims on graceful shutdown. That needs handler cancellation at
  shutdown, which changes drain behaviour.
- A fencing token for handlers. A separate sub-issue of #3050 covers it.
