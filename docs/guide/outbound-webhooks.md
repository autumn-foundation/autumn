# Outbound Signed Webhooks

Autumn has first-class support for outbound signed webhook delivery, allowing your application to dispatch structured event payloads to external API consumers safely and reliably.

## Architectural Overview

The outbound webhook subsystem consists of five core components:
1. **`WebhookSubscription`**: Represents a consumer's registered endpoint, signing secret, the event topics they are interested in, and their active status.
2. **`OutboundWebhookStore`**: A pluggable trait for persisting subscriptions and tracking delivery attempts. There is no default — `OutboundWebhookPlugin::new(store)` takes the store as a required argument, so you always choose one explicitly. `InMemoryOutboundWebhookStore` ships with the framework, but it is process-local: its subscriptions and delivery logs are lost on restart and are not shared between replicas, so it is for tests, development, and single-process deployments only. A multi-replica or production app needs a durable shared implementation of the trait. (`OutboundWebhookStore` and `InMemoryOutboundWebhookStore` are aliases kept for compatibility; the current names are `OutboundWebhookHandler` and `InMemoryOutboundWebhookHandler`.)
3. **`WebhookOutboundManager`**: The central coordinator available via `AppState` extensions, providing the `.dispatch()` method, which writes a delivery-log row per subscription and then enqueues the delivery job. The two steps are ordered, not atomic: no transaction spans the pluggable store and the job queue, so a crash between them can leave a logged event that was never enqueued. This is not an outbox guarantee, and it cannot be turned into one by reconciling after the fact — a *successful* enqueue writes no marker on the log row, so a row that was enqueued is indistinguishable from one whose process died first. A sweeper over those rows must either re-enqueue (duplicating deliveries) or skip (dropping them); it cannot tell which is correct. If you need stronger delivery guarantees, implement `OutboundWebhookHandler` over storage that can commit the log write and the enqueue together, and design receivers to be idempotent on the event ID regardless — retries mean deliveries are at-least-once even on the happy path. An enqueue that *fails* is the one case handled for you: the log row is marked `is_dlq` so it can be replayed from the DLQ endpoints below.
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

By default, Autumn provides `InMemoryOutboundWebhookStore`—a bounded, thread-safe, in-memory store ideal for tests, development, or lightweight deployments.

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

* **Exponential Backoff**: Retries are scheduled with progressive delays: $Delay = Base \times 2^{attempt-1} + Jitter$.
* **Jitter**: A randomized offset between $0.0$ and $10.0$ seconds is injected to prevent thundering herd congestion on receiving servers.
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

    // Dispatch transactionally to all matching active subscribers
    manager.dispatch(&state, "order.created", &order).await?;

    Ok(Json(order))
}
```
