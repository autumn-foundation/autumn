# Rate limiting

Autumn ships a single global token-bucket rate limiter (`[security.rate_limit]`,
keyed by client IP, authenticated principal, or API token) plus a per-route
`#[throttle]` attribute for endpoints that need a stricter limit than the
default. Both share the same limiter backend (memory or Redis), the same
keying strategies (#794), and the same `429 Too Many Requests` response shape
(`Retry-After`, `x-ratelimit-limit`, `x-ratelimit-remaining`,
`x-ratelimit-reset`, `application/problem+json` body).

## The global limiter

Enable in `autumn.toml`:

```toml
[security.rate_limit]
enabled = true
requests_per_second = 10.0
burst = 20
key_strategy = "ip"                 # "ip" (default) | "authenticated_principal" | "api_token"

# Multi-replica: share the budget across all pods.
backend = "redis"                   # "memory" (default) or "redis"
on_backend_failure = "fail_open"    # "fail_open" (default) or "fail_closed"

[security.rate_limit.redis]
url = "redis://redis:6379"
key_prefix = "myapp:rate_limit"
```

This limiter applies the *same* ceiling to every route. That's the right
default for browsing traffic — but it leaves abuse-prone endpoints (login,
password reset, signup, expensive search/export, webhook receivers) either
punishingly slow or wide open, depending on the number.

## Per-route throttling with `#[throttle]`

Add `#[throttle]` alongside `#[get]` / `#[post]` to bound requests to a
single handler on top of the global limiter. A request denied by *either*
limiter returns `429`.

### Inline form

```rust
use autumn_web::prelude::*;

#[post("/login")]
#[throttle(limit = 5, per = "1m", key = "ip")]
async fn login(Form(input): Form<LoginForm>) -> AutumnResult<Redirect> {
    // At most 5 login attempts per minute per client IP.
    do_login(input).await
}
```

`limit = N` sets the requests allowed per window (and doubles as the burst
capacity). `per = "1m"` sets the window (accepts `s`, `m`, `h`, `d` suffixes).
The steady-state refill rate is `limit / per`, so the window naturally
"resets" as tokens refill — a client denied at second 60 can succeed again
around second 72 for a `limit = 5, per = "1m"` bucket.

`key = "ip" | "principal" | "token"` selects the bucket key. Omitting `key`
defaults to whatever `key_strategy` the global limiter uses.

| `key` value | Bucket key | Fallback when absent |
|-------------|-----------|----------------------|
| `"ip"` | Connection peer / trusted-proxy-resolved client IP | — |
| `"token"` | `Authorization: Bearer <token>` value | client IP |
| `"principal"` | `RateLimitPrincipal` extension inserted by auth middleware | client IP |

### Named form (config-driven)

Move the policy out of code so ops can tune it without a recompile:

```toml
[security.rate_limit.named.login]
limit = 5
per = "1m"
key = "ip"           # optional; defaults to the global key_strategy
```

```rust
#[post("/login")]
#[throttle("login")]
async fn login(Form(input): Form<LoginForm>) -> AutumnResult<Redirect> {
    do_login(input).await
}
```

Two handlers that both use `#[throttle("login")]` share a single token bucket
(useful when e.g. `POST /login` and `POST /login/2fa` should be combined for
the same abuse budget). Inline `#[throttle(limit = …, per = …)]` forms get an
isolated per-handler bucket.

If the named entry is missing at runtime (typo, config not yet deployed), the
limiter fails **open** with a `WARN` log — the route continues to serve
traffic and the global limiter still applies.

## Composition rules

- The global limiter runs as an outer tower layer; the `#[throttle]` guard
  runs inside the handler entry. A request denied by *either* returns `429`
  with the standard headers.
- The per-route bucket is independent of the global bucket. Exceeding the
  per-route limit does not consume from the global bucket, and vice versa.
- `RateLimitExempt` still wins: requests carrying that request-extension
  marker (set by the framework's MCP envelope) bypass both the global limiter
  and any `#[throttle]` guard, so an already-charged `tools/call` is not
  double-counted on replay.
- When the limiter backend is unavailable (e.g. Redis outage), per-route
  throttles honor the global `on_backend_failure` setting: `fail_open` lets
  the request through, `fail_closed` returns `429` until the backend
  recovers.

## Limitations

`#[throttle]` — like the sibling `#[secured]` / `#[step_up]` guards it mirrors —
runs inside the handler after `FromRequestParts` extractors, but body extractors
(`Json` / `Form` / `Multipart`) are parsed by Axum *before* the throttle check.
An over-limit client can therefore still incur request-body parsing before
receiving its `429`. For hard pre-body protection (rejecting the request before
its body is read), pair the throttle with the global limiter layer, which runs
as an outer tower layer ahead of body extraction.

## Response shape

Blocked requests receive:

- HTTP status `429 Too Many Requests`
- `Retry-After: <seconds>` — when to retry
- `x-ratelimit-limit: N` — the effective bucket burst
- `x-ratelimit-remaining: 0` — always zero on a denial
- `x-ratelimit-reset: <unix-seconds>` — when the next token arrives
- `Content-Type: application/problem+json`
- RFC 9457 Problem Details body (`type` =
  `"https://autumn.dev/problems/rate-limited"`)

Clients can (and should) back off using `Retry-After`.

## Testing

`TestApp` drives requests through the same layer stack the real server uses,
including `#[throttle]` guards:

```rust
use autumn_web::config::AutumnConfig;
use autumn_web::test::TestApp;
use autumn_web::{post, routes, throttle};

#[post("/login")]
#[throttle(limit = 2, per = "1s", key = "ip")]
async fn login() -> &'static str { "ok" }

#[tokio::test]
async fn login_is_throttled() {
    let mut config = AutumnConfig::default();
    config.security.rate_limit.enabled = true;
    config.security.rate_limit.trust_forwarded_headers = true;
    let client = TestApp::new().routes(routes![login]).config(config).build();

    client.post("/login").header("X-Forwarded-For", "203.0.113.1").send().await.assert_status(200);
    client.post("/login").header("X-Forwarded-For", "203.0.113.1").send().await.assert_status(200);
    client.post("/login").header("X-Forwarded-For", "203.0.113.1").send().await.assert_status(429);
}
```

## When to reach for what

| Situation | Reach for |
|-----------|-----------|
| Blanket "no more than N req/s across the whole app" | `[security.rate_limit]` (global) |
| Stricter per-endpoint budget (login, search, export, webhook receiver) | `#[throttle(limit = …, per = …)]` |
| Policy needs to be tuned by ops without a recompile | `[security.rate_limit.named.<name>]` + `#[throttle("name")]` |
| Tier-based quotas (free vs. pro plan) | `[security.rate_limit.tiers.<name>]` + `RateLimitLayer::with_tier_hook(...)` |
| Path-prefix override on the global limiter | `RateLimitLayer::with_path_override(...)` |
| Concurrency ceiling / load shedding rather than request rate | See `docs/guide/load-shedding.md` (unreleased) |
| Failed-login lockout (attempt count, not rate) | `[auth.lockout]` — see `docs/guide/authentication.md` |
