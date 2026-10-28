# ADR 0015: Fencing Tokens Through a Lease-Row Lock

- Status: Accepted
- Date: 2026-10-05
- Deciders: Autumn maintainers
- Tags: distributed-systems, coordination, postgres, fencing

## Context

`Lock` (ADR 0010) is a session advisory lock. If the holder's connection
drops, `PostgreSQL` frees the lock. The holder's code keeps running, and a
second replica can take the lock. Neither side can detect the overlap: no
lock in the framework issues a token that a resource can check (issue #3053).

Session advisory locks also fail behind a transaction-mode pooler.

## Decision

1. Add `LeaseLock`, a lease over one row per name in `autumn_lease_locks`
   (`name`, `owner`, `generation`, `expires_at`, `acquired_at`).
   - Acquire is one upsert. It increments `generation` only if the lease is
     free or expired, and returns it as a `FencingToken` (a `u64` newtype).
   - Renew extends `expires_at` only if the generation matches and the lease
     is not yet expired. An expired lease cannot come back to life.
   - Release sets `owner = NULL` only if the generation matches. The row
     stays, so the generation never goes down.
   - All lease times use the database `now()`. App wall clocks do not decide
     who holds the lease.
2. The token is stable for one grant. Renewal does not increment it, so one
   holder can write more than once with one token. The resource check is
   `stored <= incoming`.
3. A background task renews every `ttl / 3`. The holder marks the lease lost
   when a renewal finds it gone, or when no renewal succeeds within
   `ttl - ttl / 3` of the last successful send. The local clock is only used
   for this local deadline, which ends before the database expiry.
4. `lease_lost()` is a cancellation signal. `try_with`, `with` and
   `with_timeout` drop the closure on loss and return `LockError::LeaseLost`.
5. Keep `Lock` as it is. Document it as mutual exclusion for efficiency, not
   correctness.
6. Log one boot warning per target when the database target points to a
   well-known pooler, and document a compatibility matrix.
7. Prove the protocol in Verus (`verification/lease_fencing.rs`): tokens are
   strictly monotonic per name, a stale token cannot renew or release, and a
   stale holder's write is rejected after a newer holder wrote.

## Alternatives

- **Add a token to `Lock`.** A token without a lease still frees silently on
  disconnect, and needs a table anyway. Rejected.
- **Increment the generation on renew** (the issue's first sketch). The token
  would change while one holder writes, and the holder's own earlier writes
  would look stale. Rejected.
- **Redis lease (Redlock).** Needs Redis, and has the same clock problems that
  fencing exists to avoid. Rejected.
- **Probe the pooler with `pg_backend_pid()` twice.** At boot there is no
  concurrent traffic, so a transaction pooler returns the same backend. The
  probe gives false negatives and a false sense of safety. The URL hint is
  cheap and honest about being a hint. Rejected for now.

## Consequences

- One query per held lease every `ttl / 3`.
- `generation` is a `BIGINT`, so it cannot overflow in practice.
- The scheduler now records each tick as a row with a `generation` (#3052).
  Job claims (#3051) can reuse the same pattern for their fencing tokens.
- With asynchronous replication, a failover can lose the last grant and give
  its token again. Strict fencing then needs synchronous replication.
- Dropping or truncating the table resets tokens to 1. Resources then reject
  every new holder. The `down.sql` says so.
- `LeaseLock` is Postgres only. The `SQLite` tier is single host and keeps its
  own lease `Lock`.
