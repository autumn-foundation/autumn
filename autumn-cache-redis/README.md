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
`autumn-web` version. See [docs/plugins.md](../docs/plugins.md#installing-a-plugin).

### Manual install

If you would rather wire it yourself (or `autumn plugin add` could not find your
builder chain and printed these lines for you):

```toml
[dependencies]
autumn-web        = { version = "0.4", features = ["redis"] }
autumn-cache-redis = "0.4"
```

## Quick Start

```rust,ignore
use autumn_cache_redis::RedisCache;

#[autumn_web::main]
async fn main() {
    let cache = RedisCache::from_config(&config.redis)
        .await
        .expect("Redis connection established");

    autumn_web::app()
        .with_cache(cache)
        .run()
        .await;
}
```

## Configuration

Add a `[redis]` section to `config/default.toml`:

```toml
[redis]
url = "redis://127.0.0.1:6379"
```

The URL follows the standard Redis URL format and is passed directly to the `redis` crate's
connection manager.

## Status

This crate is the first-party Redis cache plugin for `autumn-web`. It targets the same
`autumn-web` version it is published alongside.
