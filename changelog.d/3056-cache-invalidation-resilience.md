### Fixed

- **cache:** `RedisCache` no longer drops a failed `DEL` (issue #3056). Each
  invalidation retries with bounded, jittered backoff (`InvalidationRetry`,
  default 3 attempts). The async methods return the final error. The sync
  `invalidate` and `clear` log it with `warn!` and count it in the new
  `autumn_cache_invalidation_failures_total` counter.
- **cache:** a `#[repository]` with `invalidates(...)` now drops the declared
  cached reads when each write ends, after its commit (issue #3056). This also
  runs when the write returns `Err`, panics, or is cancelled: a write can
  commit and then fail in an `after_*` hook. You no longer need to
  call `invalidate_declared_caches()`. In this process, a reader that read the
  old row before the commit cannot put it back in the cache. With `commit_hooks`, the durable
  runner also invalidates, so a crash after the commit is covered.

### Changed

- **cache:** each generated write with `invalidates(...)` now awaits one
  namespace sweep per declared read after commit (issue #3056). On Redis this
  is a `SCAN MATCH`, so write latency grows with the keyspace. `with_lock`
  invalidates too; `find_or_create_by_*` invalidates only when it creates a
  row; the scheduled retention sweep invalidates when it deletes a row. With
  `commit_hooks`, the durable runner sweeps again. A failed sweep is logged and
  counted; it does not fail the write or the hook row.
- **cache:** `coherence::invalidate_namespace` now logs `warn!` and counts
  `autumn_cache_invalidation_failures_total` when the backend cannot drop the
  namespace. A custom backend that keeps the default `invalidate_namespace`
  logs this on each such write. Override it, or remove the `invalidates(...)`
  edge.

### Added

- **cache:** `Cache::invalidate_async` and `Cache::invalidate_namespace_async`
  return `Result<(), InvalidationError>` (issue #3056). Both have defaults, so
  a custom backend still compiles. `RedisCache` overrides them with real async
  calls: no `block_in_place`, and they work on a current-thread runtime. Reads
  and inserts still need a multi-thread runtime. Also
  `coherence::invalidate_namespace_async` and the generated
  `invalidate_declared_caches_async()`.
- **cache:** `GetOrComputeOptions::stale_if_error(window)` (RFC 5861, issue
  #3056). When a fill fails, the caller gets the last value for up to `window`
  after it went stale. Counted in
  `autumn_cache_read_through_stale_if_error_serves_total`.
