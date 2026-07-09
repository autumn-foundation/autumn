# ADR 0010: Expose an App-Facing Distributed Lock

- Status: Accepted
- Date: 2026-07-09
- Deciders: Autumn maintainers
- Tags: distributed-systems, coordination, postgres, advisory-locks

## Context

Autumn already runs Postgres advisory locks in three internal places:
`#[scheduled]` leader election, migration serialization, and ISR revalidation.
Each of those helpers (`try_pg_advisory_lock`, `unlock_pg_advisory_lock`,
`advisory_lock_key`) is private to its own module. Application code that needs
"run this exactly once across the cluster right now" — a nightly cleanup sweep,
cache warming, a one-shot backfill, "send the daily digest once" — has no
supported API.

The `bookmarks-distributed` example proved the demand: it hand-rolled
`pg_try_advisory_lock` / `pg_advisory_unlock` with raw `diesel::sql_query`, and
its own comments flagged the footgun — advisory locks are **session-scoped**, so
the lock-bearing connection must not be recycled to the pool or the lock
silently leaks. That is exactly the kind of subtle, data-corrupting concurrency
detail a framework should own, not copy-paste into every multi-node app.

Peer frameworks vary: Laravel ships this as a first-class feature
(`Cache::lock('name', 10)->get(fn () => ...)` / `->block($seconds, ...)`); Rails
leans on the `with_advisory_lock` gem; Django and axum/actix/loco ship nothing.

## Decision

Promote the capability Autumn already trusts in production into a small, safe
public API: `autumn_web::lock::Lock`.

- `Lock::new(pool, name)` / `Lock::from_state(&state, name)` build a lock bound
  to a **primary** pool.
- String names hash (SHA-256, first 8 bytes, big-endian) to a stable signed
  64-bit key via `distributed_lock_key`, under a `"autumn:lock:v1"` domain
  prefix that keeps application keys out of the scheduler, migration, ISR, and
  repository-upsert keyspaces.
- Acquisition comes in a blocking variant (`lock`, server-side
  `pg_advisory_lock`), a bounded blocking variant (`lock_timeout`, polling with
  a typed `LockError::Timeout` on expiry), and a non-blocking variant
  (`try_lock`, returns `None` immediately when another node holds it).
- `with` / `with_timeout` / `try_with` wrap a closure and auto-release the lock
  when the guarded section ends — normal return, early `?`, or panic.
- The lock-bearing connection is **detached from the pool** (`Object::take`) for
  the lifetime of the `LockGuard`. Explicit `release` issues
  `pg_advisory_unlock` and then closes the session; an implicit drop (early
  return or panic unwind) closes the session, which Postgres treats as releasing
  every session-scoped advisory lock it held. Either way the connection is never
  returned to the shared pool while the lock is held, so the leak the example
  warned about cannot occur in application code.

Acquisition happens on the primary connection so all replicas contend on the
same server; under sharded repositories the lock lives on whichever primary the
supplied pool targets, and callers name locks per logical resource.

## Non-Goals

This is a **coordination** lock, not a durable mutual-exclusion queue:

- **No fairness.** Postgres advisory locks are not FIFO; waiters are not served
  in arrival order, and we will not pretend otherwise.
- **No leases.** No heartbeat/renewal; if the holder's connection drops, the
  lock releases. Long-lived leader election remains the scheduler's job.
- **No row-level locking.** Pessimistic `with_lock` and optimistic locking cover
  per-row contention; this is a *named*, row-independent lock.
- **Postgres only.** Advisory-lock semantics assume Postgres. A backend trait
  for Redis or others can come later.

## Consequences

- Application code gets Laravel-grade ergonomics for run-once-across-replicas
  work while staying dependency-free on the Postgres pool Autumn already owns.
- The `bookmarks-distributed` link-checker is rewritten onto the primitive,
  deleting its raw-SQL advisory-lock code; zero
  `diesel::sql_query("...advisory_lock...")` calls remain in `examples/**`.
- Each held lock consumes one physical connection for its duration (closed on
  release). This is the conservative, correct trade the example already made;
  short critical sections make it a non-issue, and it removes any chance of
  recycling a lock-bearing connection.
- The internal scheduler/migration/ISR call sites are unchanged for now; the
  shared key-namespacing convention leaves room to converge them onto the public
  core path in a later slice.

See the [distributed locks guide](../guide/distributed-locks.md) for usage.
