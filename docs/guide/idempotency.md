# Idempotency Keys

Idempotency keys let clients safely retry mutating HTTP requests (POST, PUT,
PATCH, DELETE) without causing duplicate side-effects. The client picks a unique
key, attaches it as an `Idempotency-Key` request header, and replays the
identical request if it suspects the first attempt was lost in transit. Autumn
intercepts subsequent requests with the same key and replays the cached response
instead of re-executing the handler.

This follows the IETF draft `draft-ietf-httpapi-idempotency-key-header`.

---

## Quick start

Enable the middleware with a single builder call:

```rust,no_run
#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![create_order])
        .idempotent()   // ← opt-in
        .run()
        .await;
}
```

`.idempotent()` activates the middleware with the defaults from `autumn.toml`
(or the built-in defaults if no `[idempotency]` section is present).

Clients send the header with any UUID or opaque string:

```
POST /orders HTTP/1.1
Content-Type: application/json
Idempotency-Key: 01926b3e-dead-beef-0000-aabbccddeeff

{"item": "widget", "qty": 2}
```

The first call executes the handler and caches the response. Every subsequent
call with the **same key and identical body** receives the cached response
immediately, with an extra header:

```
HTTP/1.1 200 OK
X-Idempotent-Replayed: true
```

---

## Configuration (`autumn.toml`)

```toml
[idempotency]
enabled   = true
backend   = "memory"   # "memory" | "redis"
ttl_secs  = 86400      # how long to cache responses (default: 24 h)
in_flight_ttl_secs = 86400 # Redis safety expiry for active in-flight locks

# Memory backend: allow in production (off by default, see below)
allow_memory_in_production = false

[idempotency.redis]
# Required when backend = "redis"
url        = "redis://redis:6379/0"   # or set AUTUMN_IDEMPOTENCY__REDIS__URL
key_prefix = "autumn:idempotency"     # Redis key namespace
```

Environment overrides:

| Variable | Overrides |
|---|---|
| `AUTUMN_IDEMPOTENCY__ENABLED` | `idempotency.enabled` |
| `AUTUMN_IDEMPOTENCY__BACKEND` | `idempotency.backend` |
| `AUTUMN_IDEMPOTENCY__TTL_SECS` | `idempotency.ttl_secs` |
| `AUTUMN_IDEMPOTENCY__IN_FLIGHT_TTL_SECS` | `idempotency.in_flight_ttl_secs` |
| `AUTUMN_IDEMPOTENCY__REDIS__URL` | `idempotency.redis.url` |
| `AUTUMN_IDEMPOTENCY__REDIS__KEY_PREFIX` | `idempotency.redis.key_prefix` |

### Defaults

| Setting | Default |
|---|---|
| `enabled` | `false` (opt-in) |
| `backend` | `"memory"` |
| `ttl_secs` | `86400` (24 hours) |
| `in_flight_ttl_secs` | `86400` (24 hours) |
| `allow_memory_in_production` | `false` |
| `redis.key_prefix` | `"autumn:idempotency"` |

---

## Backends

### Memory

The in-process store is zero-config and great for development and testing. It
does **not** share state across replicas, so retries routed to a different
instance will re-execute the handler.

Autumn refuses to start with `backend = "memory"` in a production profile
unless you explicitly set `allow_memory_in_production = true`. This is a
deliberate safety check — if you omit the flag you get a clear startup error
rather than silent duplicate processing in production.

### Redis

The Redis backend uses SET EX for cached responses and SET NX EX for
distributed in-flight locks, so it coordinates correctly across multiple
replicas.

```toml
[idempotency]
enabled = true
backend = "redis"

[idempotency.redis]
url = "redis://redis:6379/0"
```

Requires the `redis` Cargo feature on `autumn-web`.

---

## Response behaviour

| Condition | Status | Extra header |
|---|---|---|
| First request for a key | handler's status | — |
| Repeat with same body | cached status | `X-Idempotent-Replayed: true` |
| Repeat with different body | `422 Unprocessable Entity` | — |
| Concurrent duplicate (first still in-flight) | `409 Conflict` | `Retry-After: 1` |
| No `Idempotency-Key` header | handler's status | — |
| Non-mutating method (GET, HEAD) | handler's status | — |

**Only successful 2xx and 3xx responses are cached.** Redirect-after-post
responses such as `303 See Other` are treated as successful mutation outcomes.
If a handler returns an error (5xx, 4xx), the entry is not stored and the next
attempt re-executes the handler — allowing transient failures to be retried
freely.

Responses that modify the Autumn `Session` are cached after the outer
`SessionLayer` saves the session and appends `Set-Cookie`. A retry after a lost
login, checkout flash, or session rotation receives the finalized cached
response instead of re-entering the mutating handler.

Routes mounted through `AppBuilder::merge()` or `AppBuilder::nest()` are raw
Axum escape hatches and are opaque to Autumn. `.idempotent()` records the first
successful raw-router mutation, but cache hits fail closed with `409 Conflict`
instead of rerunning the raw handler or replaying a stale success around
route-local auth, tenant, or audit layers. If a raw router needs successful
replay, apply `IdempotencyLayer::replay_through_inner()` inside the raw router
and place `IdempotencyReplayLayer` in the route stack after the checks that must
still run on replay.

Cached responses are scoped to a principal. The storage key folds in the
cookie-backed session (so one user's stored mutation is never replayed to
another) and, when `[tenancy] enabled = true`, the tenant Autumn's tenancy
middleware resolved for the request — so two tenants that pick the same
`Idempotency-Key` for the same route never share a cached response. The tenant
comes from the framework's own resolution rather than from the request header,
so a genuine retry from the same tenant still replays.

Manually constructed `Route` values passed to `AppBuilder::routes()` or
`AppBuilder::scoped()` are also treated as unknown by default. Autumn records
the first successful mutation, but cache hits fail closed with `409 Conflict`
unless the route explicitly opts into `RouteIdempotency::ReplayThroughInner`
and installs `IdempotencyReplayLayer` after any route-local checks that must run
again before replay.

Generated repositories with durable `after_*_commit` hooks also receive the
framework-scoped idempotency key in `MutationContext::idempotency_key` when the
repository is extracted from an idempotent HTTP request. Autumn uses that key to
de-duplicate durable commit-hook queue rows for duplicate request attempts.

### Payload mismatch (422)

If a client sends the same key with a different request body, it almost
certainly indicates a client bug. The middleware rejects it with
`422 Unprocessable Entity` immediately — the stored response is never returned.

### Concurrent duplicates (409)

When the first request is still being processed (in-flight), any duplicate
arriving at the same time receives `409 Conflict` with a `Retry-After: 1`
header. The client should retry after the suggested delay; once the first
request completes it will find the cached response.

---

## Observability

### Metrics

The `/actuator/metrics` endpoint exposes three counters under the
`idempotency` key:

```json
{
  "idempotency": {
    "hits":      12,
    "misses":    48,
    "conflicts": 0
  }
}
```

- **hits** — requests served from cache (replayed).
- **misses** — first-time requests (handler executed).
- **conflicts** — concurrent duplicates rejected with 409.

### Tracing

The middleware emits `tracing::debug!` events with structured fields:

```
idempotency.key   = "01926b3e-dead-beef-0000-aabbccddeeff"
idempotency.replayed = true
```

Pipe your log subscriber into a structured exporter (OTLP, JSON) to query
these fields in your observability backend.

---

## Startup validation

Autumn validates the idempotency configuration at startup and exits with a
clear error message if the config is invalid:

- **Memory backend in production** without `allow_memory_in_production = true`
  → startup aborts.
- **Redis backend** with no URL configured (and no `AUTUMN_IDEMPOTENCY__REDIS__URL`
  environment variable) → startup aborts.

---

## Testing

Use the `TestApp` builder in tests — it exposes an `.idempotent()` method that
enables the middleware with an in-process memory store:

```rust,no_run
use autumn_web::test::TestApp;
use autumn_web::{post, routes};

#[tokio::test]
async fn duplicate_post_replays() {
    #[post("/orders")]
    async fn create() -> &'static str { "created" }

    let client = TestApp::new()
        .routes(routes![create])
        .idempotent()
        .build();

    let r1 = client
        .post("/orders")
        .header("idempotency-key", "test-key-1")
        .send()
        .await;
    r1.assert_ok();
    assert_eq!(r1.header("x-idempotent-replayed"), None);

    let r2 = client
        .post("/orders")
        .header("idempotency-key", "test-key-1")
        .send()
        .await;
    r2.assert_ok();
    assert_eq!(r2.header("x-idempotent-replayed"), Some("true"));
}
```

You can also instantiate the layer directly against a raw axum `Router` for
lower-level tests that need finer-grained control over the store. The direct
form below should only wrap simple routes where no route-local middleware must
run again before replay:

```rust,no_run
use std::{sync::Arc, time::Duration};
use autumn_web::idempotency::{IdempotencyLayer, MemoryIdempotencyStore};

let store = Arc::new(MemoryIdempotencyStore::new(Duration::from_secs(3600)));
let layer = IdempotencyLayer::new(store.clone() as Arc<_>);

let app = axum::Router::new()
    .route("/echo", axum::routing::post(handler))
    .layer(layer);
```

---

## Low-level API

When you need a custom backend (e.g. DynamoDB, Postgres advisory locks), implement the `IdempotencyStore` trait:

```rust,ignore
use autumn_web::idempotency::{
    IdempotencyEntry, IdempotencyRecord, IdempotencyStore, IdempotencyStoreError,
};
use std::time::Duration;

struct MyStore { /* ... */ }

impl IdempotencyStore for MyStore {
    fn get(&self, key: &str) -> Option<IdempotencyEntry> { /* ... */ }

    fn set(&self, key: &str, record: IdempotencyRecord, body_hash: Vec<u8>, ttl: Duration) {
        /* ... */
    }

    fn try_set(
        &self,
        key: &str,
        record: IdempotencyRecord,
        body_hash: Vec<u8>,
        ttl: Duration,
    ) -> Result<(), IdempotencyStoreError> {
        self.set(key, record, body_hash, ttl);
        Ok(())
    }

    fn try_lock(&self, key: &str, lock_ttl: Duration) -> bool { /* true = lock acquired */ }

    fn unlock(&self, key: &str) { /* ... */ }
}
```

Wire it into the layer and apply it to your router:

```rust,ignore
let store = Arc::new(MyStore::new()) as Arc<dyn IdempotencyStore>;
let layer = IdempotencyLayer::new(store).with_ttl(Duration::from_secs(3600));

autumn_web::app()
    .routes(routes![handler])
    .layer(layer)
    .run()
    .await;
```

---

## See also

- [Middleware guide](./middleware.md) — custom Tower layers and ordering.
- [Testing guide](./testing.md) — `TestApp` and test helpers.
- [Cloud-native guide](./cloud-native.md) — Redis backend configuration for production.
