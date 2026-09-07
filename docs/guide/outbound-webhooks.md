# Outbound Signed Webhooks

Autumn has first-class support for outbound signed webhook delivery, allowing your application to dispatch structured event payloads to external API consumers safely and reliably.

## Architectural Overview

The outbound webhook subsystem consists of five core components:
1. **`WebhookSubscription`**: Represents a consumer's registered endpoint, signing secret, the event topics they are interested in, and their active status.
2. **`OutboundWebhookStore`**: A pluggable trait for persisting subscriptions and tracking delivery attempts. There is no default — `OutboundWebhookPlugin::new(store)` takes the store as a required argument, so you always choose one explicitly. `InMemoryOutboundWebhookStore` ships with the framework, but it is process-local: its subscriptions and delivery logs are lost on restart and are not shared between replicas, so it is for tests, development, and single-process deployments only. A multi-replica or production app needs a durable shared implementation of the trait. (`OutboundWebhookStore` and `InMemoryOutboundWebhookStore` are aliases kept for compatibility; the current names are `OutboundWebhookHandler` and `InMemoryOutboundWebhookHandler`.)
3. **`WebhookOutboundManager`**: The central coordinator available via `AppState` extensions, providing the `.dispatch()` method, which writes a delivery-log row per subscription and then enqueues the delivery job. The two steps are ordered, not atomic: no transaction spans the pluggable store and the job queue, so a crash between them can leave a logged event that was never enqueued. This is not an outbox guarantee, and it cannot be turned into one by reconciling after the fact — a *successful* enqueue writes no marker on the log row, so a row that was enqueued is indistinguishable from one whose process died first. A sweeper over those rows must either re-enqueue (duplicating deliveries) or skip (dropping them); it cannot tell which is correct. **Implementing the trait does not close this window.** `OutboundWebhookHandler` exposes storage methods only; `dispatch()` calls `log_delivery` and performs the enqueue *afterwards*, outside the trait, so no implementation can bring the enqueue into its own transaction. Closing the window inside the manager would require a framework change. Three more facts bear on any attempt to build a stronger guarantee on top of this, and all three are easy to get wrong:

- **A successful `dispatch()` does not mean the delivery is durable.** On the default `jobs.backend = "local"` the job queue is in-process and explicitly non-durable — a crashed process loses the queue — so `Ok` means only "handed to a queue that may not survive a restart". A durable job backend (`postgres` or `redis`) *and* a durable `OutboundWebhookHandler` are both preconditions for reasoning about loss at all.
- **Autumn transmits no stable event or delivery identifier.** Nothing Autumn sends distinguishes a first attempt from a retry of the same event: the `Autumn-Signature` header's `t=` timestamp is recomputed on every attempt but is neither unique nor stable — it is a whole-second `Utc::now().timestamp()`, and nothing spaces the attempts far enough apart to guarantee it differs. On the `local` backend it demonstrably does not: equal jitter puts the first retry 500-1000 ms after the failure (see *Retries* below), so two attempts routinely fall in the same second and produce a byte-identical signature. No header or envelope field carries an event or delivery ID. (Other headers may be present — under `telemetry-otlp` the shared HTTP client injects W3C `traceparent`/`tracestate`.) If a receiver must deduplicate, the application has to mint a stable ID, put it in the payload, and reuse it verbatim on every retry.
- **Retries can duplicate an event only after the job is enqueued.** Before that point the loss window above applies, so `dispatch()` is neither at-least-once nor at-most-once on its own. Idempotent receivers protect you from duplicates; nothing here protects you from loss.

This page deliberately stops short of prescribing an exactly-once design. Getting one right depends on the job backend, the handler implementation, and how the application sequences its own transaction against `dispatch()` — and each of those changes the answer. If you need that guarantee, treat the points above as the constraints to design against, and take the design itself to the maintainers rather than inferring it from this page. One case is handled for you, on the default path only: when `dispatch()` falls back to enqueuing the `autumn_webhook_delivery` job and that enqueue fails, the log row is marked `is_dlq` and can be replayed from the DLQ endpoints below. If a `WebhookDelegateExt` is installed, `dispatch()` hands the delivery to that delegate *instead* of enqueuing, and a delegate error is returned to the caller without marking the row — so a failed delegated delivery never appears in the DLQ, and an operator looking there will not find it.
4. **`autumn_webhook_delivery` Job**: A resilient background job that handles HTTP POST delivery, computes payload signatures, executes retries, and handles deactivations.
5. **Actuator Operations**: Sensitive API endpoints under `/actuator/webhooks/*` for monitoring the Dead Letter Queue (DLQ) and replaying permanently failed deliveries.

---

## 1. Webhook Subscriptions

A subscription is registered for a set of event topics and points to a target destination URL:

```rust
use autumn_web::webhook_outbound::{WebhookSubscription, WebhookSubscriptionStatus};

let subscription = WebhookSubscription {
    id: "sub_123".to_owned(),
    target_url: "https://api.consumer.com/webhooks/receiver".to_owned(),
    event_topics: vec!["order.created".to_owned(), "order.fulfilled".to_owned()],
    secret: "whsec_stripe_style_signing_secret_key_32_bytes!!".to_owned(),
    status: WebhookSubscriptionStatus::Active,
    consecutive_failures: 0,
};
```

### Subscription Statuses
* **`Active`**: The subscription is operational; events will be dispatched immediately.
* **`Disabled`**: The subscription is manually turned off by the operator or consumer.
* **`Failed`**: The subscription has been **automatically deactivated** after exceeding the maximum failure threshold (50 consecutive failures) to protect your application resources and avoid thundering herd requests on failing external servers.

---

## 2. Pluggable Storage Backend

To support diverse hosting environments, the persistence layer is abstracted behind the `OutboundWebhookStore` trait. You can implement this trait to store subscription states and delivery logs in PostgreSQL, Redis, MongoDB, or any external service.

Autumn ships `InMemoryOutboundWebhookStore`—a bounded, thread-safe, in-memory implementation—but it is not a default: `OutboundWebhookPlugin::new(store)` requires you to name a store, so choosing it is always explicit. Because it is process-local, its subscriptions and delivery logs are lost on restart and are not shared between replicas. Use it for tests, local development, and single-process apps whose webhook state is genuinely disposable. Anything with more than one replica — or that must survive a deploy — needs a durable implementation of the trait.

---

## 3. Stripe-Style Payload Signing

Security is established using Stripe-style HMAC-SHA256 payload signing. Every request body is signed using the subscription's secret key and sent via the `Autumn-Signature` header in the format:

```http
Autumn-Signature: t=1778930400,v1=a1b2c3d4e5f6...
```

* **`t`**: The Unix epoch timestamp of the delivery dispatch.
* **`v1`**: The computed HMAC-SHA256 hex signature of the string `{timestamp}.{raw_body}`.

### Verification (Consumer side)
The consumer receives the header, extracts `t` and `v1`, concatenates `t` and the raw request body bytes with a `.`, computes the HMAC-SHA256 using their registered secret, and compares it securely with `v1` to prevent timing attacks.

---

## 4. Retries, Jitter, and the Dead Letter Queue (DLQ)

If a webhook delivery fails (network exception, connection timeout, or a non-2xx HTTP status code), the background job initiates a robust retry flow:

* **Exponential Backoff**: every backend computes `initial_backoff_ms * 2^(attempt-1)`, so the default 1000 ms base gives 1 s, 2 s, 4 s, 8 s (`local_retry_delay_ms`, `redis_retry_delay_ms`, `pg_retry_delay_ms` in `autumn/src/job.rs`).
* **Jitter — on the `local` backend only.** `execute_local_job` passes that delay through `jittered_retry_delay_ms`, which applies **equal jitter**: it *reduces* the delay to a random point in `[delay/2, delay]`, so the first retry lands 500-1000 ms after the failure. Half is guaranteed so a retry can never fire near-instantly, and the delay never exceeds the un-jittered value, so it cannot push a retry past an existing timeout budget. The durable backends do **not** jitter — `redis` and `postgres` schedule the exact exponential delay — so the same app retries at 500-1000 ms in development and at exactly 1 s in production.
* **Capped Attempts**: Delivery is retried up to a maximum of **5 attempts**.
* **Dead Letter Queue (DLQ)**: If all 5 attempts fail, the delivery log is permanently archived as `is_dlq = true` and retired from active background processing.

---

## 5. Actuator Observability and Replaying

Operators can monitor the state of outbound webhooks and manually trigger re-deliveries of DLQ logs using the sensitive actuator routes:

### List DLQ Deliveries
Retrieve all permanently failed webhook delivery attempts:
```http
GET /actuator/webhooks/dlq
```
**Response (JSON)**:
```json
[
  {
    "id": "log_456",
    "subscription_id": "sub_123",
    "topic": "order.created",
    "payload": "{\"order_id\":\"ord_999\"}",
    "request_headers": {
      "Content-Type": "application/json",
      "Autumn-Signature": "t=1778930400,v1=..."
    },
    "response_status": 500,
    "response_body": "{\"error\":\"Internal Server Error\"}",
    "elapsed_ms": 142,
    "attempt": 5,
    "max_attempts": 5,
    "is_dlq": true,
    "last_error": "server returned status: 500",
    "timestamp": "2026-05-26T05:00:00Z"
  }
]
```

### Replay a DLQ Log
Manually reset and trigger re-delivery of a dead-lettered log. The system will reset the attempt counter back to 1, mark the log as no longer in the DLQ, and re-enqueue a fresh background delivery task:
```http
POST /actuator/webhooks/replay
Content-Type: application/json

{
  "log_id": "log_456"
}
```

---

## 6. AppBuilder Integration

To enable outbound webhooks, configure your store and register the `OutboundWebhookPlugin` in your application setup:

```rust
use std::sync::Arc;
use autumn_web::prelude::*;
use autumn_web::webhook_outbound::{InMemoryOutboundWebhookStore, OutboundWebhookPlugin};

#[autumn_web::main]
async fn main() {
    let store = Arc::new(InMemoryOutboundWebhookStore::new());
    let webhook_plugin = OutboundWebhookPlugin::new(store.clone())
        .with_initial_backoff_ms(1000); // 1s base retry backoff

    autumn_web::app()
        .plugin(webhook_plugin)
        .run()
        .await;
}
```

---

## 7. Dispatching Events

Within your HTTP handlers or workflow tasks, extract the `WebhookOutboundManager` from application extensions to dispatch structured payloads:

```rust
use autumn_web::prelude::*;
use autumn_web::webhook_outbound::WebhookOutboundManager;

#[post("/orders")]
async fn create_order(
    state: State<AppState>,
    Json(payload): Json<CreateOrderPayload>,
) -> AutumnResult<Json<Order>> {
    let order = save_order_to_db(&payload).await?;

    // Fetch the manager from app extensions
    let manager = state.extension::<WebhookOutboundManager>()
        .ok_or_else(|| AutumnError::internal_server_error_msg("Webhook outbound subsystem not registered"))?;

    // Logs a delivery row and enqueues a delivery job for each matching active
    // subscriber. The two steps are not atomic: a crash between them can drop
    // the event, and no retry will recover it. Once enqueued, retries can
    // deliver more than once, so receivers must be idempotent.
    manager.dispatch(&state, "order.created", &order).await?;

    Ok(Json(order))
}
```
