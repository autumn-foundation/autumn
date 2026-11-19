### Changed

- **idempotency:** `IdempotencyTx::commit` and `set_recovery_point` write the
  key row once instead of twice. The crash-safe expiry (one TTL past a live
  lock) is now part of the record write, so a database-backed idempotent
  request issues one statement fewer and writes one row version fewer, with
  the same stored expiry. See `docs/perf/idempotency-db-request/`.
