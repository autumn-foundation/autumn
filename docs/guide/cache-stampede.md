# Cache Stampede Protection

When a hot cache key expires, every concurrent request misses at once and
recomputes the same value — one expiry turns into a database load spike.
`get_or_compute` / `get_or_compute_with` add read-through fills to Autumn's
cache abstraction that coalesce those concurrent misses, so a hot-key expiry
degrades to **one recompute** instead of a thundering herd.

## Why it matters

Autumn's cache already exposes `get`/`insert`/`invalidate`, but nothing
coalesces concurrent misses: in-process callers race each other to refill the
same key, and across replicas N processes each refill the same Redis key. The
read-through API fixes both:

- **In-process:** concurrent misses for the same key run the fill once per
  replica; every other caller awaits that one fill and shares its result.
- **Cross-replica (opt-in, Redis):** a distributed fill lock ensures at most
  one replica fills a hot key at a time, and/or stale-while-revalidate serves
  the last-known-good value while one replica refreshes in the background.

## Quick start

```rust,ignore
use autumn_web::cache::{get_or_compute, Cache};
use std::sync::Arc;
use std::time::Duration;

async fn cached_bookmark_count(cache: &Arc<dyn Cache>) -> Result<i64, sqlx::Error> {
    get_or_compute(cache, "bookmark_count", Some(Duration::from_secs(30)), || async {
        count_bookmarks_in_db().await
    })
    .await
    .map_err(|e| e.into_fill().expect("fill error is the only variant reachable here"))
}
```

On a hit, the cached value returns immediately. On a miss, the first caller
runs the closure; every concurrent caller for the same key awaits that one
fill instead of recomputing it themselves.

For cross-replica protection, use `get_or_compute_with` with a Redis-backed
cache:

```rust,ignore
use autumn_web::cache::{get_or_compute_with, GetOrComputeOptions};
use std::time::Duration;

let opts = GetOrComputeOptions::new()
    .ttl(Duration::from_secs(30))
    .distributed_fill_lock(true);           // opt-in: requires a backend that
                                             // implements Cache::try_acquire_fill_lock
                                             // (RedisCache does)

let count: i64 = get_or_compute_with(&cache, "bookmark_count", opts, || async {
    count_bookmarks_in_db().await
})
.await?;
```

## In-process vs distributed protection

| | In-process single-flight | Distributed fill lock |
|---|---|---|
| Scope | one replica | across all replicas sharing the backend |
| Mechanism | a process-global in-flight registry keyed by `(cache, key)`; concurrent callers await a `tokio::sync::watch` channel published by whichever caller became the "leader" | `Cache::try_acquire_fill_lock` (Redis: `SET NX PX` + a Lua compare-and-delete release) |
| Opt-in? | always on | `GetOrComputeOptions::distributed_fill_lock(true)` |
| Backend support | any `Cache` | any backend implementing the two lock methods (Redis, out of the box) |

**Per-replica divergence note:** the in-process registry only ever sees
requests on *its own* replica. With the default in-process Moka cache and no
distributed lock, N replicas each independently coalesce their own concurrent
misses — so a hot-key expiry still produces up to N fills (one per replica),
just not N × (requests per replica). This mirrors the existing divergence
between per-replica Moka caches described in the [cloud-native
guide](cloud-native.md#shared-cache): if you need a single fill *cluster-wide*,
enable the Redis backend and the distributed fill lock.

When the lock is contended, the losing replica polls the cache (at
`lock_poll_interval`, default 50ms) for the winner's value and also
re-attempts the lock (picking up a crashed holder once its lock TTL expires).
If neither happens within `lock_wait_timeout` (default 5s), the replica gives
up waiting and fills locally — bounded extra work, never unavailability.

## Stale-while-revalidate

`GetOrComputeOptions::stale_while_revalidate(grace)` trades a bounded window
of staleness for zero-latency reads at expiry:

```rust,ignore
let opts = GetOrComputeOptions::new()
    .ttl(Duration::from_secs(30))
    .stale_while_revalidate(Duration::from_secs(300));
```

- While the value is **fresh** (within `ttl`), reads return it immediately.
- Once **stale** (past `ttl` but within `ttl + grace`), reads still return the
  last-known value immediately, and kick off **at most one** background
  refresh per key (through the same single-flight registry — concurrent stale
  reads never start a second refresh).
- Once past `ttl + grace`, the key is treated as a cold miss again.

**Trade-offs:** callers may observe a value up to `grace` old. The fill
closure passed to `get_or_compute_with` must be `'static` (it may run on a
background task rather than the calling task) — capture owned handles
(`Arc`-clone your pool) rather than borrows. On the in-process Moka backend,
per-entry physical expiry isn't available (Moka's TTL is per-cache-instance);
freshness is tracked in a stored envelope instead, so correctness doesn't
depend on Moka evicting the entry.

## Failure semantics

A failing fill never poisons the key:

- Nothing is written to the cache on error.
- The caller that ran the fill gets a typed `CacheFillError::Fill(e)`.
- Every coalesced waiter gets `CacheFillError::FillFailed(message)` — the
  leader's error rendered via `Display` (the error type isn't required to be
  `Clone`).
- The in-flight entry is removed before waiters are notified, so the **next**
  caller retries the fill from scratch.
- If a leader is cancelled or panics mid-fill, the in-flight entry is removed
  (RAII), and any waiters see their channel close and re-contend for
  leadership instead of hanging forever.
- The distributed lock's `lock_ttl` bounds the damage from a filler that
  crashes while holding it: the lock self-clears and another replica takes
  over.

## De-synchronizing mass expiry

Keys written together in a batch (a bulk warmup, a scheduled refresh) tend to
expire together too, turning a single expiry into a stampede across many keys
at once. `jittered_ttl` spreads them out:

```rust
use autumn_web::cache::jittered_ttl;
use std::time::Duration;

// Each key's TTL is uniformly randomized within ±20% of 5 minutes.
let ttl = jittered_ttl(Duration::from_secs(300), 0.2);
```

## Metrics

The read-through API updates process-wide counters, visible on
`/actuator/metrics` (under `"cache"`) and `/actuator/prometheus`:

| Prometheus name | Meaning |
|---|---|
| `autumn_cache_read_through_hits_total` | fast-path reads served from a fresh value |
| `autumn_cache_read_through_misses_total` | fast-path reads that found no fresh value |
| `autumn_cache_read_through_coalesced_waits_total` | callers that awaited a concurrent in-process fill |
| `autumn_cache_read_through_fills_total` | fill closures that completed successfully |
| `autumn_cache_read_through_fill_failures_total` | fill closures that returned an error |
| `autumn_cache_read_through_stale_serves_total` | stale-while-revalidate reads served while a refresh ran |
| `autumn_cache_fill_lock_acquires_total` | distributed fill locks acquired |
| `autumn_cache_fill_lock_contended_total` | distributed fill lock attempts that found the lock held elsewhere |

**Falsifiable success metric:** K concurrent requests hitting an expired key
should record 1 fill and K−1 coalesced waits. With the Redis distributed fill
lock enabled across replicas, total fills across all replicas should be 1.

## See also

- [Data caching and `#[cached]`](cloud-native.md#shared-cache) — function-level
  memoization and the Redis backend
- [Maud fragment caching](fragment-caching.md) — caching rendered view fragments
- [Plugin Metrics Sources](metrics-sources.md) — the `MetricsSource` extension
  point for app- and plugin-contributed metrics
