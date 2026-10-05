# Distributed Locks

Some work must run on **exactly one replica at a time**: a nightly cleanup
sweep, warming a cache, a one-shot data backfill, or "send the daily digest
once". `autumn_web::lock::Lock` gives you a named, cluster-wide lock for those
critical sections without hand-rolling Postgres advisory locks or reasoning
about connection lifetimes.

It is the same advisory-lock machinery Autumn already trusts in production to
gate its own migrations and ISR revalidation — promoted into a small, safe
public API. (`#[scheduled]` uses a tick table instead; see
[scheduled-multi-replica.md](scheduled-multi-replica.md).)

> **Two locks, two jobs.** `Lock` gives mutual exclusion for *efficiency*: it
> stops duplicate work in the normal case. It does not give *correctness*. If
> the holder's connection drops, the lock is free, and the holder's code keeps
> running. When a second holder must not corrupt data, use
> [`LeaseLock`](#fencing-tokens-leaselock) and check its fencing token at the
> resource.

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
  connection drops, the lock releases. A network failure,
  `idle_session_timeout` or a primary failover releases the lock. The holder
  gets no signal, and its code continues. For long-lived leader election, use the
  [multi-replica scheduler](scheduled-multi-replica.md). (On SQLite it *is* a
  lease — see below.)
- **Not for correctness.** `Lock` is mutual exclusion for efficiency. It issues
  no token, so a resource cannot detect two holders. Use
  [`LeaseLock`](#fencing-tokens-leaselock) and a fenced write when overlap
  corrupts data.
- **Not row-level.** For per-row contention use pessimistic `with_lock` or
  optimistic locking; this is a *named*, row-independent lock.

## Fencing tokens: `LeaseLock`

No lock can stop a paused or disconnected holder. A GC pause, a VM freeze or a
lost connection can outlast any lease. The holder then wakes up and writes,
while a second holder also writes. The fix is a **fencing token**: a number
that increases with each grant, which the resource checks on each write
([Kleppmann, "How to do distributed locking"](https://martin.kleppmann.com/2016/02/08/how-to-do-distributed-locking.html)).

`LeaseLock` is a lease over a row in `autumn_lease_locks`:

- Each grant of one lock name gets a strictly larger `FencingToken`. The row
  stays after release, so the token never goes down.
- The holder renews the lease in the background every third of its TTL
  (`with_lease_ttl`, default 30s, floor 1s). The token does not change on
  renewal.
- Every lease time comes from the database `now()`. App wall clocks do not
  decide who holds the lease. The holder uses its local monotonic clock only
  to stop early.
- Each acquire, renew and release is one autocommit statement. The lock holds
  no connection while held.
- If a renewal finds the lease gone, or no renewal succeeds for two thirds of
  the TTL, the lease is *lost*. `lease_lost()` resolves, and `is_lost()`
  returns `true`. The database gives the lease to a new holder only after the
  full TTL. Thus the old holder gets the signal first.

```rust
use autumn_web::prelude::*;

async fn rebuild_report(state: &AppState) -> AutumnResult<()> {
    let lock = LeaseLock::from_state(state, "report-rebuild")?;
    let ran = lock
        .try_with(|lease| async move {
            // Send lease.fencing_token() with every write. See below.
            let body = format!("rebuilt under token {}", lease.fencing_token());
            write_report(state, &body, lease.fencing_token()).await
        })
        .await?;
    match ran {
        Some(result) => result,
        None => Ok(()), // Another replica holds the lease.
    }
}
```

`try_with`, `with` and `with_timeout` stop the closure when the lease is lost,
and return `LockError::LeaseLost` (a 503). For manual control, `try_lock`,
`lock` and `lock_timeout` return a `LeaseGuard`. Watch `guard.lease_lost()` in
a `tokio::select!`, and call `guard.release().await` at the end.

The table is created on first use. The framework migration
`20261005143012_create_lease_locks` creates it too, for a role without
`CREATE`.

`LeaseLock` is Postgres only. It is not available under the `sqlite` feature.

Limits:

- **Failover.** With asynchronous replication, a failover can lose the last
  grant. The new primary can then give the same token to a second holder, and
  `<=` accepts both. Use synchronous replication when this matters.
- **Do not reset the table.** A truncated or recreated `autumn_lease_locks`
  starts again at token 1. Each resource that stored a higher token then
  rejects every new holder.
- **Cancelled acquire.** If you cancel `try_lock` after the database grants the
  lease, no holder has it until it expires (one TTL). `lock_timeout` can run
  past its budget by one in-flight query for the same reason.
- **Database clock.** A large forward step of the database clock can end a
  lease early. The token still protects the resource.

### Check the token at the resource

The lease only tells the holder that it *probably* still holds the lock. The
resource makes the decision. Store the highest token that you accepted, and
write only when the incoming token is not lower:

```sql
ALTER TABLE reports ADD COLUMN fencing_token BIGINT NOT NULL DEFAULT 0;
```

```rust
use autumn_web::prelude::*;
use autumn_web::reexports::diesel;
use autumn_web::reexports::diesel_async::RunQueryDsl as _;

async fn write_report(state: &AppState, body: &str, token: FencingToken) -> AutumnResult<()> {
    let pool = state
        .pool()
        .ok_or_else(|| AutumnError::service_unavailable_msg("no database pool"))?;
    let mut conn = pool.get().await?;
    let rows = diesel::sql_query(
        "UPDATE reports SET body = $1, fencing_token = $2 \
         WHERE id = 1 AND fencing_token <= $2",
    )
    .bind::<diesel::sql_types::Text, _>(body)
    .bind::<diesel::sql_types::BigInt, _>(token.as_i64())
    .execute(&mut conn)
    .await?;
    if rows == 0 {
        // A newer holder wrote first. This holder is stale: stop.
        return Err(AutumnError::conflict_msg("stale fencing token"));
    }
    Ok(())
}
```

Use `<=`, not `<`. One holder can write more than once with one token. A lower
token is a stale holder, and the `WHERE` rejects it.

For a resource that is not SQL (a file, an object store, an external API),
keep the highest token next to the data, and use
`stored.admits(incoming)` (on `FencingToken`) to make the same decision.

The model and its proofs are in `verification/lease_fencing.rs` (Verus). They
show that tokens are strictly monotonic per lock name, that a stale token
cannot renew or release, and that a stale holder's write is rejected after a
newer holder wrote. They prove the tokens, not the timing: the model does not
show that two holders never overlap. The token is what makes overlap safe.

## Connection poolers

A transaction-mode pooler (PgBouncer `pool_mode = transaction`, Amazon RDS
Proxy, Supabase Supavisor on port 6543, the Neon `-pooler` endpoint) gives each
transaction a different server session. A session advisory lock stays on a
session that the app no longer owns: it leaks, or it frees at a random time.

| Feature | Session pooling or direct | Transaction pooling |
| --- | --- | --- |
| `Lock` (advisory) | Works | **Not safe.** Use `LeaseLock`. |
| `LeaseLock` | Works | Works, if the pooler supports prepared statements (see below) |
| Postgres `#[scheduled]` coordinator | Works | Works. Each tick is a row, not a session lock. |
| Migrations (`autumn migrate`, auto-migrate) | Works | **Not safe.** Run migrations on a direct URL. |

Autumn sends each query as a named prepared statement. In transaction mode,
the pooler must keep prepared statements across server sessions: PgBouncer
1.21 or later with `max_prepared_statements` above 0. Older poolers fail with
`prepared statement "s0" does not exist`.

To run migrations on a direct URL, set `auto_migrate = false` under
`[database]`. Then run `autumn migrate` with `DATABASE_URL` set to the direct
host, not to the pooler.

Autumn cannot ask a pooler for its mode. At boot, the pool builder reads the
database URL and logs one warning when it finds a well-known pooler: port 6432
or 6543, a `pgbouncer` host, `pgbouncer=true`, a `*.proxy-*.rds.amazonaws.com`
host, a `*.pooler.supabase.com` host on port 6543, or a Neon `-pooler` host.
It reads URLs and libpq `key=value` strings, and warns once per target. This
is a hint only. It does not find a pooler on a plain host name and port, it
does not read multi-host URLs, and a custom `DatabasePoolProvider` skips it.

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

See [ADR 0010](../adr/0010-app-facing-distributed-lock.md) and
[ADR 0015](../adr/0015-fencing-lease-lock.md) for the design rationale.
