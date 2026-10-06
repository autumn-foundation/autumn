### Breaking Changes

- **Breaking:** `IdempotencyStore` is async ([migration guide](docs/migrations/next.md)).
  Each method returns an `IdempotencyFuture` and a `Result` (issue #3061). The
  trait has four methods: `get`, `set`, `try_lock(key, owner, ttl)` and
  `unlock(key, owner)`. `try_get`, `try_set`, `try_lock_owned` and
  `unlock_owned` are removed. The Redis store does not call `block_in_place`,
  so it works on a current-thread runtime and does not hold a Tokio worker
  while it waits for Redis.
- **Breaking:** `idempotency.in_flight_ttl_secs` defaults to `60`, not `86400`
  ([migration guide](docs/migrations/next.md)). `IdempotencyLayer::new` uses
  60 s too, not the response TTL. When the record write fails after the
  handler, a retry gets `409` for one minute, not one day.

### Added

- **idempotency:** `DbIdempotencyStore` keeps records in the app database
  (Postgres, or SQLite under `sqlite`). Select it with
  `[idempotency] backend = "database"`. It needs the new framework migration
  `autumn_idempotency_keys`.
- **idempotency:** the `IdempotencyTx` extractor. `IdempotencyTx::commit`
  writes the response in the handler's `Db::tx`, so the response commits with
  the mutation. A crash after the commit no longer re-runs the mutation: when
  the in-flight lock expires, the retry replays the committed response. If a
  request's lock expired and another request took the key, `commit` returns
  `409` and the transaction rolls back. If the handler also changes the
  session, the key stays locked until the record holds the final
  `Set-Cookie`. Use it on a primary `Db` connection, not a shard.
- **idempotency:** `IdempotencyTx::set_recovery_point` and
  `IdempotencyTx::recovery_point` let a multi-step handler resume after a
  crash. A recovery point belongs to its request body: a retry with another
  body gets `422`.
- **idempotency:** a boot warning when `in_flight_ttl_secs` is shorter than
  `server.timeouts.request_timeout_ms`, and a boot note when no request
  timeout is set.
