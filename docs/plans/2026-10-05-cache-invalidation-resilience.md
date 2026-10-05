# Cache invalidation resilience (#3056)

Status: implemented. Part of #3050.

## Problem

1. `RedisCache::invalidate` ignores the `DEL` result. A Redis error leaves
   stale data until the TTL. The only trace is a `debug!` line.
2. Invalidation is a manual call. Nothing ties it to the commit. If the code
   invalidates before the commit, a reader can refill the old value.
3. A failed read-through fill returns an error. It does not serve the last
   known value (no `stale-if-error`, RFC 5861).
4. Each Redis call uses `block_in_place`. This panics on a current-thread
   runtime and holds a Tokio worker during Redis latency.

## Brainstorm

- Return `Result` from `invalidate`. (Breaking for every `Cache` impl.)
- Add async, fallible methods with defaults. (Not breaking.)
- Retry `DEL` with bounded, jittered backoff in `RedisCache`.
- Count final failures in `autumn_cache_invalidation_failures_total`.
- Call the generated invalidator after each generated write commits.
- Send the invalidation through the durable commit-hook queue.
- Use the existing epoch fence to stop a fill that started before the commit.
- Keep an envelope with a `fresh_until` stamp. Serve it when a fill fails.

## Reverse brainstorm: how do we make it worse?

| Way to fail | Counter-measure |
| --- | --- |
| Drop the error and log at `debug` | Retry. Then `warn!` and count the failure. |
| Retry with no limit | Limit attempts. Cap each sleep. |
| All replicas retry at the same time | Add jitter to each sleep. |
| Invalidate before the commit | Invalidate after the write's own commit. |
| A slow reader writes the old value after the invalidation | Bump the namespace epoch first. The fenced insert skips the old value. |
| Fail the write when only the cache failed | Keep the write `Ok`. Count and log the cache failure. |
| Fail a durable hook row when only the cache failed | Log and count. Do not dead-letter user hooks. |
| Skip invalidation when the write commits, then fails | Invalidate on `Ok` and on `Err`. |
| Park a runtime worker on the fill-fence lock | Wait for the lock on the blocking pool. |
| Serve stale data for ever | Limit `stale_if_error` to a window after `fresh_until`. |
| Change the `Cache` trait signature | Add new methods with defaults. Keep the old ones. |
| Block a Tokio worker | Use async methods on the framework path. |

## Six thinking hats

- **White (facts).** Each generated write commits its own transaction. The
  in-process epoch fence exists (`with_fill_fence`). The durable queue exists
  for repositories with `commit_hooks`. CI runs `autumn-cache-redis` Docker
  tests and the `integration_tests` Docker sweep.
- **Red (feelings).** A silent stale cache erodes trust in `cache audit`. Users
  expect a declared edge to run.
- **Black (risks).** A namespace sweep on Redis is a `SCAN`. Each write now
  pays for one sweep per declared read. The fence is per process. A fill on
  another replica can still write an old value.
- **Yellow (benefits).** A declared edge now runs with no extra code. Errors
  are visible in metrics and logs. Reads survive a database outage.
- **Green (ideas).** Use `REPLICAOF` to make a real Redis reject `DEL`
  (`READONLY`). This gives a reversible fault with no proxy.
- **Blue (process).** Write failing tests first (RED). Then implement (GREEN).
  Then clean up and document (REFACTOR).

## Design

1. `Cache` gets `invalidate_async` and `invalidate_namespace_async`. Both
   return `Result<(), InvalidationError>`. The defaults call the sync methods.
2. `RedisCache` overrides both with real async code. It retries with
   `InvalidationRetry` (default: 3 attempts, 20 ms base, 200 ms cap, ±50 %
   jitter). The sync `invalidate` uses the same retry. On final failure it
   logs `warn!` and calls `record_invalidation_failure`.
3. `coherence::invalidate_namespace_async` bumps the epoch, clears local
   stores, and awaits the backend. A failure logs `warn!`, increments the
   counter, and returns `false`.
4. A repository with declared edges calls
   `invalidate_declared_caches_async()` when each write method ends, on `Ok`
   and on `Err` (a write can commit, then fail in an `after_*` hook). A guard
   spawns it on a panic or a cancelled request. With `commit_hooks`, the
   durable runners also invalidate, before the user hook. A failure there is
   logged and counted. It does not fail the row: a cache outage must not
   delay or dead-letter user hooks.
5. `GetOrComputeOptions::stale_if_error(window)` stores an envelope. When a
   fill fails and `now < fresh_until + window`, the caller gets the old value.
   `autumn_cache_read_through_stale_if_error_serves_total` counts this.

## Tests

| Criterion | Test |
| --- | --- |
| `DEL` failure surfaced or retried | `autumn-cache-redis` Docker tests: `redis_invalidate_async_surfaces_readonly_del_failure`, `redis_invalidate_retries_until_del_succeeds`, `redis_invalidate_namespace_async_surfaces_failure`, `redis_sync_invalidate_counts_final_failure`, `coherence_async_invalidation_reaches_redis_and_reports_failure` |
| After-commit invalidation, no refill of the old value | `autumn/tests/integration/cache_invalidation_after_commit.rs` (Docker): `each_generated_write_invalidates_after_commit_with_no_manual_call`, `a_reader_that_read_before_the_commit_cannot_repopulate_the_old_value`, `a_write_that_commits_then_fails_still_invalidates`, `a_reader_inside_the_transaction_window_cannot_keep_the_old_value` |
| `stale_if_error` serves the old value | `autumn/tests/integration/cache_stampede.rs` |

## Out of scope

- Async `get` and `insert` on `Cache`. The read path still uses
  `block_in_place` on Redis.
- A cross-replica fence. The fence is per process.
- Reads from a lagging read replica after the invalidation.
- `MutationContext::invalidate_keys`: nothing reads it yet.
