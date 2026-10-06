# Health Indicators

Autumn's `/actuator/health` and `/ready` endpoints surface the framework's own
state (startup, graceful shutdown, a primary database ping). The
`HealthIndicator` trait lets any component — a payment gateway, a feature-flag service, an SMTP relay,
a downstream HTTP API — **plug its own health check** into those same endpoints.

This mirrors Spring Boot's `HealthIndicator` model. Autumn adapts it for async
Rust: the `check()` method returns a `BoxFuture`, every indicator runs with a
per-indicator timeout (default 2 s), and registration is explicit via
`AppBuilder`.

---

## Quick start

### 1. Implement `HealthIndicator`

```rust
use autumn_web::actuator::{HealthCheckOutput, HealthIndicator, HealthStatus};
use std::collections::HashMap;

pub struct StripeIndicator {
    // your HTTP client, config, etc.
}

impl HealthIndicator for StripeIndicator {
    fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
        Box::pin(async move {
            // Try a lightweight Stripe API call (e.g. list balance)
            match self.ping_stripe().await {
                Ok(_) => HealthCheckOutput::up(),
                Err(e) => {
                    let mut details = HashMap::new();
                    details.insert("error".to_string(), serde_json::json!(e.to_string()));
                    HealthCheckOutput {
                        status: HealthStatus::Down,
                        details,
                    }
                }
            }
        })
    }
}
```

### 2. Register with `AppBuilder`

```rust
use std::sync::Arc;

autumn_web::app()
    .routes(routes![...])
    .health_indicator("stripe", Arc::new(StripeIndicator::new()))
    .run()
    .await;
```

### 3. Verify it appears in `/actuator/health`

```bash
curl http://localhost:3000/actuator/health | jq .
```

```json
{
  "status": "UP",
  "version": "0.5.0",
  "profile": "dev",
  "uptime": "12s",
  "components": {
    "stripe": {
      "status": "UP"
    }
  }
}
```

---

## Status precedence

Overall status follows Spring Boot precedence (most-severe wins):

| Condition | Overall status | HTTP code |
|-----------|---------------|-----------|
| Any indicator is `DOWN` | `DOWN` | 503 |
| Any `OUT_OF_SERVICE`, no `DOWN` | `OUT_OF_SERVICE` | 503 |
| Any `UNKNOWN`, no failures | `UNKNOWN` | 200 |
| All `UP` (or no indicators) | `UP` | 200 |

The built-in `db` check participates in the same aggregation.

---

## Readiness vs health-only

By default an indicator gates **both** `/ready` and `/actuator/health`
(`IndicatorGroup::Readiness`). A Kubernetes deploy is blocked until the
indicator is healthy.

To contribute to `/actuator/health` only — without blocking rolling deploys —
override the `group()` method:

```rust
use autumn_web::actuator::IndicatorGroup;

impl HealthIndicator for StripeIndicator {
    fn group(&self) -> IndicatorGroup {
        IndicatorGroup::HealthOnly   // does not gate /ready
    }
    // ...
}
```

**When to use `HealthOnly`**: payment gateways, analytics sinks, non-critical
notification services. A degraded Stripe connection doesn't mean the app can't
serve requests.

**When to use `Readiness`** (default): databases your app can't function
without, feature-flag services that gate core flows, cache layers local to this
replica. For a dependency that all replicas share, read
[Fail open on shared dependencies](#fail-open-on-shared-dependencies).

---

## Per-indicator timeout

Each indicator runs with a timeout. If `check()` does not resolve in time the
indicator is reported as `UNKNOWN` with `timed_out: true` in its `details`.
The default is 2 000 ms. Override it per-indicator:

```rust
impl HealthIndicator for SlowExternalService {
    fn timeout_ms(&self) -> u64 { 5_000 }   // 5 s for a slow upstream

    fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
        Box::pin(async move { /* ... */ })
    }
}
```

A timed-out indicator in the `/actuator/health` response:

```json
{
  "components": {
    "slow_service": {
      "status": "UNKNOWN",
      "details": { "timed_out": true }
    }
  }
}
```

---

## Hiding details in production

When `health.detailed = false` (the default in `prod` profile), the
per-component `details` map is **omitted** from the response. The `status`
field is always present.

```toml
# autumn-prod.toml
[health]
detailed = false
```

---

## Registering from a plugin

Plugins use the same `AppBuilder` API inside their `build()` method:

```rust
use autumn_web::plugin::Plugin;
use autumn_web::app::AppBuilder;
use std::sync::Arc;

pub struct PaymentsPlugin { /* ... */ }

impl Plugin for PaymentsPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.health_indicator("payments", Arc::new(PaymentsHealthIndicator::new()))
    }
}
```

This means `autumn-admin-plugin` or any future plugin can contribute health
indicators without requiring app glue code.

---

## Built-in indicators

| Name | Feature flag | Group | What it checks |
|------|-------------|-------|----------------|
| `db` | `db` | Readiness | `SELECT 1` on the primary database |
| `redis:<subsystem>` | `redis` | HealthOnly (see below) | `PING` to the Redis of one subsystem |

### `db`

The `db` check sends `SELECT 1` to the primary on one dedicated connection.
That connection is not in the pool, so a fully checked-out pool cannot block
the ping. A ping that fails, or does not finish in `health.ping_timeout_ms`,
is `DOWN`. Then `/ready` returns `503`. A late ping is `DOWN`, not `UNKNOWN`.

A busy pool does not make `/ready` fail. If it did, the load balancer would
move the load to the other app replicas. Then those replicas would become busy
and fail too. To shed load, set `server.max_concurrent_requests`. It is off by
default. When on, excess requests get `503` with `Retry-After`.

The primary is shared by all app replicas, but it is an exception to
[Fail open on shared dependencies](#fail-open-on-shared-dependencies): an app
replica that cannot reach it usually has its own network problem. To keep app
replicas in rotation when the primary fails, set `health.db_readiness = false`.
Then `/ready` does not ping the primary, so a hung primary cannot delay it.
`/actuator/health` still pings it and shows `db` as `DOWN`.

The generated Dockerfile `HEALTHCHECK` probes `/health`, the readiness alias.
ECS and Docker Swarm replace a container that fails it. On those platforms,
set `AUTUMN_HEALTHCHECK_URL=http://localhost:3000/live`, so a primary outage
does not restart every container.

Count the ping connection in your connection budget: one per app replica, and
one more for a read replica. A connection pooler in front of Postgres (for
example, PgBouncer in transaction mode) queues the ping like other queries. A
saturated pooler can then make the ping late on all app replicas at the same
time. Point `database.url` at the pooler only when its queue stays short.

The output appears in both `components.db` and the legacy `checks.database`
key. The details keep the pool numbers (`pool_size`, `active_connections`,
`idle_connections`). With `health.detailed = true`, they also show the ping
`error`. The error text can name the host or the user, so a body that is not
detailed does not show it.

The read-replica check and the `db:shard:<name>` indicators use the same
dedicated-connection ping.

### `redis:<subsystem>`

The framework registers one indicator for each subsystem that runs on Redis:
`cache`, `channels`, `idempotency`, `jobs`, `rate_limit`, `sessions`,
`submit_token` and `webhook_replay`. A subsystem that is off gets none (for
example, idempotency without `enabled = true`, webhook replay with no
replay-protected endpoint, or jobs when the app has no jobs). `RedisCachePlugin`
registers `redis:cache` when it installs the Redis cache, with
`RedisHealthIndicator::shared`, so it shares the connection of the other
indicators on the same URL. Without the plugin,
`cache.backend = "redis"` gets no indicator. `rate_limit` follows
`security.rate_limit.backend = "redis"` also when the global limiter is off,
because `#[throttle]` routes use that backend. A `worker` replica serves no user
routes, so it gets no indicator for sessions, idempotency, submit tokens, rate
limiting or webhook replay. Each sends `PING` with the
`health.ping_timeout_ms` limit. A failed or late `PING` is `DOWN`, not
`UNKNOWN`. Subsystems on one URL share one connection, and checks that run at
the same time share one `PING`. A subsystem whose backend you install with the
builder (`with_session_store`, `with_cache_backend`, `with_channels_backend`)
does not use the configured Redis, so it gets no indicator.

A kept connection (database or Redis) that does not answer in half the time
limit gets help: the check also opens a new connection and uses the first one
that answers. So a slow but healthy server stays `UP`, and a hung connection is
replaced.

They are `HealthOnly` by default. Set `health.redis_readiness = true` to make
them gate `/ready`. Read
[Fail open on shared dependencies](#fail-open-on-shared-dependencies) before
you do.

To check another Redis, register `RedisHealthIndicator` yourself. It needs
the `redis` feature:

```toml
autumn-web = { version = "0.8", features = ["redis"] }
```

```rust
use autumn_web::redis_health::RedisHealthIndicator;
use std::{sync::Arc, time::Duration};

fn reports_redis() -> autumn_web::reexports::redis::RedisResult<Arc<RedisHealthIndicator>> {
    let indicator = RedisHealthIndicator::new("redis://reports.internal:6379")?
        .with_timeout(Duration::from_millis(500));
    Ok(Arc::new(indicator))
}

// autumn_web::app().health_indicator("redis:reports", reports_redis()?)
```

---

## Result cache

Many probers (load balancers, the orchestrator, monitors) call `/ready` at
the same time. Autumn keeps each result of the database pings and of each
registered indicator for `health.cache_ttl_ms`. When a result is stale, one
refresh runs. The other probes wait for its result. The check runs a maximum
of one time in each TTL period. This is also true during an incident.

The refresh runs in its own task. A prober that disconnects does not stop it,
and the next probe gets its result.

```toml
[health]
cache_ttl_ms = 1000      # default; 0 turns the cache off
ping_timeout_ms = 2000   # default; limit for one database or Redis ping
db_readiness = true      # default; false: a failed primary ping does not gate /ready
redis_readiness = false  # default; true: Redis indicators gate /ready
```

Keep `ping_timeout_ms` below the probe timeout of your platform (for
Kubernetes, `timeoutSeconds`). Then a slow ping gives a clear `503`, not a
probe timeout. `ping_timeout_ms = 0` is refused at startup.

The cache also keeps a timed-out result. Probers do not call a hung dependency
again before the TTL ends. A change that an indicator reports (for example,
`OUT_OF_SERVICE` for a drain) shows after the TTL ends.

A registry you build with `HealthIndicatorRegistry::new()` does not cache.
Call `set_cache_ttl` to turn the cache on. `TestApp` applies
`health.cache_ttl_ms`. A test that changes an indicator and reads it again at
once must set `health.cache_ttl_ms = 0`.

---

## Fail open on shared dependencies

`/ready` tells the load balancer to stop sending traffic to **this** app
replica. That helps only when other app replicas are healthy. A dependency that
all app replicas share (one Redis, one external API) fails for all of them at
the same time. If it gates `/ready`, all app replicas leave rotation. A short
failure then becomes a full outage.

- Gate `/ready` on a dependency only when both conditions are true: this app
  replica cannot serve requests without it, and another app replica can serve
  them.
- Put shared dependencies in `HealthOnly`. Alert on them from
  `/actuator/health` and metrics.
- Use circuit breakers and fallbacks for degraded service. Circuit breakers do
  not gate `/ready`.
- Some load balancers fail open when every target is unhealthy (for example
  AWS ALB). Do not depend on this behavior.
