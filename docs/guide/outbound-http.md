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

[http.client.base_urls]
stripe   = "https://api.stripe.com"
sendgrid = "https://api.sendgrid.com"
```

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
| `502 Bad Gateway` | Immediate retry with exponential back-off (100 ms × 2ⁿ) |
| `503 Service Unavailable` | Same |
| `504 Gateway Timeout` | Same |
| `429 Too Many Requests` | Retried after the `Retry-After` header delay (1 s default) |
| Connection / timeout error | Retried up to `max_retries` times |

`POST` and `PATCH` are **not** retried by default (not idempotent). Override
per-call:

```rust
// Retry a POST up to 2 extra times (e.g. idempotent webhook delivery)
client.post(url).retries(2).send().await?;

// Disable retries entirely
client.get(url).no_retry().send().await?;
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
