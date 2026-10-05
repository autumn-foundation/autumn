### Breaking Changes

- **Breaking:** the `prod` profile sets `server.strict_config = true` ([migration guide](docs/migrations/next.md)).
  An unknown top-level table, or an unknown key in `[server]`, `[deploy]` or
  `[database]`, now stops the boot. A misspelled timeout key no longer falls
  back to the default in silence. Unknown keys in other sections log a warning
  (#3057).
- **Breaking:** `capacity::AdmissionLimit` is `#[non_exhaustive]` and has a new `ProfileDefault` variant ([migration guide](docs/migrations/next.md)) (#3057).

### Changed

- **prod profile:** load shedding is on by default. The ceiling is the primary
  pool size × 32 (`PROD_REQUESTS_PER_POOL_CONNECTION`), at least 256
  (`PROD_MIN_ADMISSION_LIMIT`). An explicit `server.max_concurrent_requests`
  or a capacity contract wins. When the contract cannot be used, the ceiling
  falls back to this value, not to unlimited. Set
  `max_concurrent_requests = 0` to turn it off (#3057).
- **prod profile:** `database.statement_timeout = "30s"` and the new
  `database.idle_in_transaction_timeout = "60s"` (Postgres builds) (#3057).
- **migrations:** each transactional migration starts with
  `SET LOCAL lock_timeout` (new `database.migration_lock_timeout`, default
  `5s`). A migration that times out on a table lock is retried with jittered
  backoff (new `database.migration_lock_retries`, default `5`). A
  `run_in_transaction = false` migration gets no timeout (#3057).

### Added

- **database:** each framework transaction (`Db::tx`, `tx_with`,
  `tx_immediate`, repository writes) starts with `SET LOCAL statement_timeout`
  and `SET LOCAL idle_in_transaction_session_timeout`, so a transaction pooler
  such as `PgBouncer` cannot drop them. A route's `StatementTimeout` applies
  to them (#3057).
- **migrations:** `MigrationLockPolicy`, `run_pending_with_policy`,
  `run_pending_locked_with_policy`, and `MigrationError::LockContention`
  (#3057).
- **config:** new env vars `AUTUMN_DATABASE__STATEMENT_TIMEOUT`,
  `AUTUMN_DATABASE__IDLE_IN_TRANSACTION_TIMEOUT`,
  `AUTUMN_DATABASE__MIGRATION_LOCK_TIMEOUT`,
  `AUTUMN_DATABASE__MIGRATION_LOCK_RETRIES`. `AUTUMN_SERVER__STRICT_CONFIG=false`
  now turns strict config off (#3057).
