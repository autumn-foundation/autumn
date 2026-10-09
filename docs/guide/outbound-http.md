# Outbound HTTP Client

Autumn ships a first-class outbound HTTP client that plugs into the same
tracing, configuration, and testing machinery as the rest of the framework.
Zero extra dependencies are needed — `Client` is available whenever you use
`autumn-web` with the default feature set.

## Quick start

Declare `Client` as a handler parameter and call third-party APIs directly:

```rust
use autumn_web::prelude::*;
use autumn_web::http::Client;

#[post("/charges")]
async fn create_charge(
    client: Client,
    Json(req): Json<ChargeRequest>,
) -> AutumnResult<Json<ChargeResponse>> {
    let resp = client
        .post("https://api.stripe.com/v1/charges")
        .header("authorization", "Bearer sk_live_…")
        .json(&serde_json::json!({
            "amount": req.amount,
            "currency": req.currency,
        }))
        .send()
        .await?;

    Ok(Json(resp.json()?))
}
```

`Client` is an Axum extractor — it reads configuration from `[http.client]` in
`autumn.toml` and, in tests, intercepts matching requests against any mocks
registered with `TestApp::http_mock`.

## Configuration

```toml
# autumn.toml
[http.client]
timeout_secs = 30     # per-request timeout (default: 30)
max_retries  = 3      # retries on idempotent methods (default: 3)
max_backoff_ms = 20000 # cap on the jittered retry backoff (default: 20000)

[http.client.base_urls]
stripe   = "https://api.stripe.com"
sendgrid = "https://api.sendgrid.com"
```

The client also obeys the request deadline and a retry budget. See
[Timeouts, Deadlines and Retry Budgets](timeouts-and-budgets.md).

Base URL aliases let you name your upstream services and reference them by
alias in handlers and tests:

```rust
// Uses the "stripe" base URL from config, prepends it to "/v1/charges"
let resp = client.named("stripe").post("/v1/charges").send().await?;
```

## Retries

By default, `GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS`, and `TRACE` (idempotent
methods) are retried up to three times on:

| Condition | Behaviour |
|---|---|
| `502 Bad Gateway`, `504 Gateway Timeout` | Retry after the jittered backoff |
| `503 Service Unavailable` | Same. With `Retry-After`, wait for the hint (see below) |
| `429 Too Many Requests` | Wait for `Retry-After` (1 s when absent, see below) |
| Connection / timeout error | Retry after the jittered backoff |

A retry starts only when the request deadline has time for it and the retry
budget has tokens for it. See
[Timeouts, Deadlines and Retry Budgets](timeouts-and-budgets.md).

**Backoff.** The wait before retry `n` (0 = first retry) is a random value in
`[0, min(max_backoff, 100 ms × 2ⁿ)]` ("full jitter"). Callers that fail
together do not retry together. `max_backoff` is `[http.client]
max_backoff_ms` (default 20 000), or `.max_backoff(d)` per call. A client
from app state (the `Client` extractor, `Client::from_state`) draws the
jitter from the app's entropy. Under a `Sim` that entropy is seeded, so a
seed replays the same delays.

**`Retry-After`.** On a `429` or `503`, the client reads `Retry-After`
(seconds or an HTTP date). It caps the hint at `max_retry_after_secs` and at
the request timeout. The wait is then `backoff + min(hint, 5 s)`. So the
retry never comes before a hint of up to 5 s, and callers that get the same
hint still spread out. A hint above 5 s is cut to about 5 s.

`POST` and `PATCH` are **not** retried by default (not idempotent).
`.retries(n)` sets the count only. Opt in per call with
`.retry_non_idempotent()`. The client then sends an `Idempotency-Key` header
with a random value, the same on every attempt, unless you set one:

```rust
// Retry a POST up to 2 extra times. The server can deduplicate on the key.
client.post(url).retries(2).retry_non_idempotent().send().await?;

// Disable retries entirely
client.get(url).no_retry().send().await?;
```

## Adaptive throttling

When a host rejects most calls, more calls only add load. The client can stop
them locally (Google SRE client-side throttling, issue #3068). It is off by
default:

```toml
[http.client.adaptive_throttle]
enabled = true
k = 2.0            # reject once accepts fall below 1/k of requests
window_secs = 120
```

For each host, the client counts attempts (`requests`) and attempts that the
host did not reject (`accepts`). A `429`, a `503` or a transport error is not
an accept. A new attempt is rejected locally with probability
`max(0, (requests − k × accepts) / (requests + 1))`. While the host accepts
more than `1/k` of the attempts, nothing is rejected. Old counts leave the
window. Thus the client sends requests to the host again when the host is
serviceable.

A rejected attempt ends the call with `ClientError::ThrottledLocally { host }`.
With `?` in a handler, it maps to `503`. A local reject is not a
circuit-breaker failure. Retries are attempts too, so the throttle also stops
a retry. Under a request deadline the client follows redirects itself, so each
redirect hop is an attempt on its own host. All `Client` extractors in an app
use one set of counts.

The custom send path (`pin_to`, `pin_to_addrs`, `get_ssrf_safe`, `no_redirect`,
`follow_redirects`) uses the throttle only with `breaker_scoped()`, once per
call, because its hosts often come from users. A throttle tracks at most 4096
hosts; it does not reject calls to a host it does not track.

## Criticality header

The client sends `X-Autumn-Criticality` with the class of the inbound request
that it serves (see [Criticality](resilience.md#criticality)). It does not
send `default`, because a missing header means `default`. Set a class for one
call:

```rust,ignore
client
    .get("https://api.example.com/report")
    .criticality(autumn_web::Criticality::Sheddable)
    .send()
    .await?;
```

## Response

```rust
let resp = client.get("https://api.example.com/users/1").send().await?;

resp.status();        // reqwest::StatusCode
resp.headers();       // &reqwest::header::HeaderMap
resp.is_success();    // true for 2xx

// Consume the body (choose one):
let value: MyType = resp.json()?;   // deserialise from JSON
let text  = resp.text();            // UTF-8 string (lossy)
let bytes = resp.bytes();           // raw Bytes
```

## Trace propagation

When the `telemetry-otlp` feature is enabled, every outbound request
automatically carries a `traceparent` header derived from the active span.
This means the inbound trace ID from a `#[handler]` or `#[job]` propagates
transparently to the upstream service — no extra wiring needed.

A `tracing::info!` event is emitted for every request with:

```
http.method, http.host, http.path, http.status, http.elapsed_ms
```

`Authorization`, `Cookie`, and `Set-Cookie` header **values** are never
included in span fields or logs — only the header names of non-sensitive
headers are recorded.

## Testing with mocks

`TestApp::http_mock` registers canned responses and lets you assert call
counts without a real network server — the mock harness is symmetric with the
`TestApp` server-side test harness.

```rust
use autumn_web::test::TestApp;
use serde_json::json;

#[tokio::test]
async fn create_charge_calls_stripe_once() {
    let mut app = TestApp::new().routes(routes![create_charge]);

    // Register a canned response for POST /v1/charges on the "stripe" alias.
    let mock = app
        .http_mock("stripe")
        .post("/v1/charges")
        .respond_with(200, json!({
            "id": "ch_test_123",
            "amount": 1000,
            "status": "succeeded",
        }));

    let client = app.build();

    client
        .post("/charges")
        .json(&json!({"amount": 1000, "currency": "usd"}))
        .send()
        .await
        .assert_status(200);

    // Assert the handler made exactly one outbound call.
    mock.expect_called(1);
}
```

`http_mock(alias)` returns a `MockSetupBuilder`. Chain a method and path, then
call `respond_with(status, json_body)` to register the entry and obtain a
`MockHandle` for later assertions.

| Method | Description |
|---|---|
| `.get(path)` | Match `GET <path>` |
| `.post(path)` | Match `POST <path>` |
| `.put(path)` | Match `PUT <path>` |
| `.patch(path)` | Match `PATCH <path>` |
| `.delete(path)` | Match `DELETE <path>` |
| `.respond_with(status, body)` | Register and return `MockHandle` |
| `.respond_with_status(status)` | Register with empty body |

`MockHandle` assertions:

```rust
mock.expect_called(1);       // panics with a diagnostic if count differs
let n = mock.call_count();   // raw count without asserting
```

If a handler makes a request that matches no registered mock while the mock
registry is active, the request returns a `ClientError::NoMock` error rather
than hitting the network — so unregistered calls are caught immediately.

## Standalone usage (outside handlers)

### In `#[scheduled]` and `#[job]` tasks

Tasks that receive `AppState` should call `Client::from_state` to borrow the shared
connection pool built once at server startup.  This avoids creating a new TCP/TLS
connection on every task invocation:

```rust
use autumn_web::http::Client;
use autumn_web::prelude::*;

#[scheduled(every = "1h", name = "link-checker")]
pub async fn check_links(state: AppState) -> AutumnResult<()> {
    let client = Client::from_state(&state);
    // client reuses the shared connection pool — no cold handshake
    client.get("https://api.example.com/status").send().await?;
    Ok(())
}
```

### Outside the framework

For truly standalone use (CLI utilities, benchmarks, tests that run without an
`AppState`), construct a client directly:

```rust
use autumn_web::http::Client;
use std::time::Duration;

// Default settings (30 s timeout, 3 retries on idempotent methods)
let client = Client::new();

// Custom timeout
let client = Client::with_timeout(Duration::from_secs(10));

// From framework config
let client = Client::from_config(&config.http.client);
```

## Complete example

See [`examples/reddit-clone/src/routes/auth.rs`](../../examples/reddit-clone/src/routes/auth.rs)
for a working example with outbound HTTP calls and integration tests covering
mocked calls, call-count assertions, and error handling.
