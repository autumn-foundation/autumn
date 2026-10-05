# autumn-cache-redis

Redis-backed shared cache plugin for [`autumn-web`](https://crates.io/crates/autumn-web) applications.

This crate provides a `RedisCache` implementation that stores cached values in Redis, suitable
for multi-process or multi-instance Autumn deployments that need a shared, externally-accessible
cache instead of the default in-process Moka store.

## Installation

```bash
autumn plugin add autumn-cache-redis
```

One command adds the dependency at a version compatible with your app's
`autumn-web`, mounts the plugin in your `autumn_web::app()` builder chain, and
prints any configuration still needed. It is safe to re-run, and it refuses —
before touching any file — to install into an app on an incompatible
`autumn-web` version. See [docs/plugins.md](https://github.com/autumn-foundation/autumn/blob/main/docs/plugins.md#installing-a-plugin).

### Manual install

If you would rather wire it yourself (or `autumn plugin add` could not find your
builder chain and printed these lines for you):

```toml
[dependencies]
autumn-web        = { version = "0.8", features = ["redis"] }
autumn-cache-redis = "0.8"
```

## Quick Start

`autumn plugin add autumn-cache-redis` writes this mount for you. It reads
`[cache]` at startup and installs the Redis cache only when
`backend = "redis"`, so it is inert until you configure it:

```rust,ignore
use autumn_cache_redis::RedisCachePlugin;

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .plugin(RedisCachePlugin::new())
        .run()
        .await;
}
```

To build the store yourself instead — bypassing the `[cache] backend` switch —
construct it and install it on the builder:

```rust,ignore
use autumn_cache_redis::RedisCache;

#[autumn_web::main]
async fn main() {
    let cache = RedisCache::from_config(&config.cache.redis)
        .await
        .expect("Redis connection established");

    autumn_web::app()
        .with_cache_backend(cache)
        .run()
        .await;
}
```

## Configuration

Select the Redis backend and point it at your server in `autumn.toml`:

```toml
[cache]
backend = "redis"

[cache.redis]
url = "redis://127.0.0.1:6379"
```

`RedisCachePlugin` reads `[cache]` at startup: until `backend = "redis"` is set
it is a no-op and the default in-process Moka cache stays in use.

The URL follows the standard Redis URL format and is passed directly to the `redis` crate's
connection manager.

## Invalidation errors

Invalidation retries a failed `DEL` with bounded, jittered backoff. The
default is 3 attempts (two sleeps, about 90 ms at most). Change it with
`RedisCache::with_invalidation_retry`:

```rust,ignore
use autumn_cache_redis::{InvalidationRetry, RedisCache};
use std::time::Duration;

let cache = RedisCache::from_config(&config.cache.redis)
    .await?
    .with_invalidation_retry(InvalidationRetry::new(
        5,                          // attempts
        Duration::from_millis(20),  // first sleep
        Duration::from_millis(500), // sleep cap
    ));
```

- `invalidate_async` and `invalidate_namespace_async` return the final error.
  They are real async calls and work on any Tokio runtime.
- The sync `invalidate` and `clear` cannot return an error. They log it with
  `warn!` and count it in `autumn_cache_invalidation_failures_total`.
- All sync methods, `invalidate` and `clear` included, use `block_in_place`.
  They need a multi-thread Tokio runtime.

## Status

This crate is the first-party Redis cache plugin for `autumn-web`. It targets the same
`autumn-web` version it is published alongside.
