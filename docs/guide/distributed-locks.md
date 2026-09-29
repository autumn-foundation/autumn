# Distributed Locks

Some work must run on **exactly one replica at a time**: a nightly cleanup
sweep, warming a cache, a one-shot data backfill, or "send the daily digest
once". `autumn_web::lock::Lock` gives you a named, cluster-wide lock for those
critical sections without hand-rolling Postgres advisory locks or reasoning
about connection lifetimes.

It is the same advisory-lock machinery Autumn already trusts in production to
gate its own migrations, `#[scheduled]` leader election, and ISR revalidation —
promoted into a small, safe public API.

## Quick start

```rust
use autumn_web::prelude::*;

#[scheduled(every = "24h", name = "nightly-cleanup")]
async fn nightly_cleanup(state: AppState) -> AutumnResult<()> {
    let lock = Lock::from_state(&state, "nightly-cleanup")?;

    // Runs on exactly one replica; the rest observe `None` and skip. The lock
    // auto-releases when the section ends — normal return, early `?`, or panic.
    let ran = lock
        .try_with(|| async {
            // ... expensive cleanup that must not run twice ...
            Ok::<(), AutumnError>(())
        })
        .await?;

    match ran {
        // We held the lock and ran the cleanup — propagate its result.
        Some(result) => result?,
        // Another replica already holds the lock and is doing the work — skip.
        None => {}
    }

    Ok(())
}
```

Use `try_with` (or `try_lock`) whenever the work **must not run twice**: the
replica that wins the lock runs the closure, and every other replica sees `None`
and skips. Reach for the blocking `with` / `with_timeout` only to *serialize* a
mutually-exclusive section where every waiter should eventually run — those
variants block until the current holder releases and then run the closure, so
they are **not** a run-once primitive.

`Lock::from_state` builds a lock from any state's **primary** pool. If you hold
a pool directly, use `Lock::new(pool, "name")`.

## Blocking vs. non-blocking

| Method | Behavior |
| --- | --- |
| `try_lock()` | Returns `Ok(None)` immediately if another node holds the lock. |
| `lock()` | Blocks (server-side) until the lock is free. |
| `lock_timeout(dur)` | Blocks up to `dur`, then returns `LockError::Timeout`. |
| `try_with(f)` | Runs `f` only if the lock is free right now; else `Ok(None)`. |
| `with(f)` | Blocks to acquire, runs `f`, releases. |
| `with_timeout(dur, f)` | Blocks up to `dur` to acquire, runs `f`, releases. |

Use `try_with` for opportunistic "whoever gets here first does the work, the
rest skip it" fan-out (this is how the `bookmarks-distributed` link-checker
claims each shard):

```rust
for shard in shard_ids() {
    let ran = Lock::from_state(&state, format!("link-checker:shard:{shard}"))?
        .try_with(|| process_shard(shard))
        .await?;

    if ran.is_none() {
        // Another replica owns this shard right now — skip it.
        continue;
    }
}
```

## Auto-release and panic safety

The lock is released when the guarded section ends, no matter how:

- **Normal return** and **early `?`** — the closure wrappers (`with` /
  `with_timeout` / `try_with`) run `pg_advisory_unlock` and recycle the
  connection back to the pool; a `LockGuard` you drop yourself force-closes its
  session instead.
- **Panic** — as the stack unwinds, the guard's `Drop` force-closes the
  lock-bearing session, which Postgres treats as releasing every session-scoped
  advisory lock it held. No leaked lock.

While the lock is held its connection stays checked out of the pool — counted
against `database.pool.max_size` and never returned to the shared pool while
held. A clean `release` runs `pg_advisory_unlock` and recycles that connection
back to the pool for reuse; a panic, cancelled future, or unlock error instead
force-closes the session. Either way a lock-bearing connection can never
silently leak the lock — the footgun you would face hand-rolling
`pg_try_advisory_lock` / `pg_advisory_unlock` yourself. Because a held lock
occupies a pool slot for its whole duration, keep critical sections short and
size the pool for the number of locks you hold concurrently.

If you need manual control, `try_lock` / `lock` return a `LockGuard`; call
`guard.release().await` to release explicitly (it surfaces a typed error on
unlock failure), or just drop it.

## Lock names and keyspaces

String names are hashed to a stable, signed 64-bit key via
`distributed_lock_key`. The same name always yields the same key; different
names differ with overwhelming probability. A `"autumn:lock:v1"` domain prefix
keeps application lock keys **out of** the keyspaces the scheduler, migrations,
ISR revalidation, and repository upserts already use, so an app lock named
`"cleanup"` cannot collide with an internal lock.

## Sharding and replica routing

Advisory locks must be taken on the **primary** so every replica contends on the
same server. `Lock::from_state` and `Lock::new` therefore acquire on the primary
connection. Under a sharded repository the lock lives on whichever primary the
pool you pass points at; use one lock name per logical resource (for example
`"link-checker:shard:{n}"`) so contention maps to the resource, not the shard
topology.

## Non-goals

This is a **coordination** lock, not a durable mutual-exclusion queue:

- **Not fair.** Postgres advisory locks are not FIFO; waiters are not served in
  arrival order.
- **Not a lease** on Postgres. There is no heartbeat/renewal; if the holder's
  connection drops, the lock releases. For long-lived leader election, use the
  [multi-replica scheduler](scheduled-multi-replica.md). (On SQLite it *is* a
  lease — see below.)
- **Not row-level.** For per-row contention use pessimistic `with_lock` or
  optimistic locking; this is a *named*, row-independent lock.

## On SQLite

The same API works under the `sqlite` cargo feature (issue #1907). SQLite has no
`pg_advisory_lock`, so a named lock is a lease row in an `autumn_locks` table in
the app's own database file. The runtime creates that table on first use.

The contract a caller relies on is the same — one holder at a time, released on
drop — with three differences:

- **The scope is one host.** Processes sharing the database file contend; two
  hosts do not. That is the SQLite tier, not this lock. See
  [SQLite in production](sqlite-in-production.md).
- **It is a lease, not a session.** A holder that dies frees the lock at the
  lease expiry (`with_lease_ttl`, default 30s) rather than wedging it, and a
  live holder renews in the background, so a long critical section is not
  preempted.
- **It is not re-entrant.** A Postgres session lock can be taken twice on one
  connection; a second `try_lock` on the same name in the same process observes
  `None`.

`lock()` and `lock_timeout()` poll rather than waiting server-side, because
there is no server-side wait to block in. Tune the interval with
`with_poll_interval`.

See [ADR 0010](../adr/0010-app-facing-distributed-lock.md) for the design
rationale.
