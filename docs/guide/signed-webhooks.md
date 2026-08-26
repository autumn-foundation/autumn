# Signed Webhook Intake

Autumn can verify third-party callbacks before your handler runs. The
`SignedWebhook` extractor preserves the exact request bytes, checks the
provider signature, applies timestamp tolerance, rejects replayed delivery IDs,
and then hands the verified bytes and metadata to your route.

## Configure endpoints

Use provider presets under `security.webhooks.endpoints`.

```toml
[security.webhooks]

[security.webhooks.replay]
# Dev/test default is "memory". Production webhook endpoints should use Redis.
backend = "redis"

[security.webhooks.replay.redis]
url = "redis://redis:6379/0"
key_prefix = "myapp:webhooks:replay"

[[security.webhooks.endpoints]]
name = "stripe"
path = "/webhooks/stripe"
provider = "stripe"
secret_env = "STRIPE_WEBHOOK_SECRET"
previous_secret_envs = ["STRIPE_WEBHOOK_SECRET_PREVIOUS"]
timestamp_tolerance_secs = 300
replay_window_secs = 86400

[[security.webhooks.endpoints]]
name = "github"
path = "/webhooks/github"
provider = "github"
secret_env = "GITHUB_WEBHOOK_SECRET"
```

Provider presets:

| Provider | Signature input | Signature header | Timestamp | Delivery/event metadata |
|----------|-----------------|------------------|-----------|-------------------------|
| `stripe` | `{timestamp}.{raw_body}` | `Stripe-Signature: t=...,v1=...` | from signature header | JSON `id` and `type` |
| `github` | raw body | `X-Hub-Signature-256: sha256=...` | none | `X-GitHub-Delivery`, `X-GitHub-Event` |
| `slack` | `v0:{timestamp}:{raw_body}` | `X-Slack-Signature: v0=...` | `X-Slack-Request-Timestamp` | JSON `event_id` and `type`; URL verification falls back to JSON `challenge` |
| `generic` | raw body | `X-Webhook-Signature: sha256=...` | optional | `X-Webhook-Delivery`, `X-Webhook-Event` |

Secrets can be set directly with `secret = "..."` for local fixtures, but use
`secret_env` in real deployments. During rotation, move the old value to
`previous_secrets` or `previous_secret_envs`; new signatures use `secret`, while
old signatures verify until you remove the previous value.

For Slack Events API callbacks, replay protection uses the `event_id` field in
the JSON callback body. URL verification requests do not include `event_id`, so
Autumn uses their `challenge` value as the one-shot replay key. For Slack-style
sources outside Events API, set `delivery_id_header` and `event_type_header`
explicitly if those identifiers arrive in headers.

Production config validation fails when a configured endpoint has no secret or
uses a weak/template value. Apps with no configured signed webhooks are
unchanged.

## Scaffold an endpoint

`autumn generate webhook <provider> <Name>` wires all of the above up for you —
the route, the `[[security.webhooks.endpoints]]` config (`secret_env`, replay
protection on), the CSRF/CAPTCHA path exemptions, an event-dispatch skeleton,
and tests covering the valid / missing / invalid / replayed delivery cases:

```bash
autumn generate webhook stripe Payments
autumn generate webhook github Repo
autumn generate webhook slack Events
autumn generate webhook generic Partner --secret-env PARTNER_WEBHOOK_SECRET
```

See `docs/guide/generators.md` for the full generator contract.

## Write a handler

```rust,ignore
use autumn_web::prelude::*;

#[post("/webhooks/stripe")]
async fn stripe(webhook: SignedWebhook) -> AutumnResult<Json<serde_json::Value>> {
    let event: serde_json::Value = webhook.json()?;

    Ok(Json(serde_json::json!({
        "received": true,
        "provider": webhook.provider(),
        "delivery_id": webhook.delivery_id(),
        "event_type": webhook.event_type(),
        "event": event,
    })))
}
```

`SignedWebhook` exposes:

- `provider()` - `stripe`, `github`, `slack`, or `generic`
- `endpoint()` - configured endpoint name
- `delivery_id()` - provider delivery ID when present
- `event_type()` - provider event type when present
- `received_at()` - server receive time used for tolerance/replay checks
- `raw_body()` - exact verified HTTP bytes
- `json<T>()` - parse the verified payload after authentication

## Error contract

Failures happen before handler business logic:

| Failure | Status | Problem Details code |
|---------|--------|----------------------|
| Missing signature, malformed signature, malformed timestamp, missing replay ID | `400 Bad Request` | `autumn.bad_request` |
| Stale timestamp or mismatched signature | `401 Unauthorized` | `autumn.unauthorized` |
| Duplicate delivery ID inside the replay window | `409 Conflict` | `autumn.conflict` |
| Replay backend unavailable | `503 Service Unavailable` | `autumn.service_unavailable` |

Responses use `application/problem+json` and include `type`, `title`, `status`,
`detail`, `instance`, `code`, `request_id`, and `errors`.

## Raw-body invariant

Signatures are verified against the exact HTTP body bytes. Do not parse JSON or
forms before verification. A payload like `{"id":"evt_1"}` and
`{"id": "evt_1"}` can deserialize to the same value but must produce different
HMAC inputs; Autumn rejects the byte-modified request before your handler runs.

## Replay protection

Replay protection is enabled by default. Autumn stores
`provider:endpoint:delivery_id` and rejects a second delivery inside
`replay_window_secs` with `409 Conflict`.

The default replay backend is `memory`, which is process-local and suitable for
tests, development, and explicitly single-replica deployments. Production
validation refuses to start replay-protected webhook endpoints on `memory`
unless you set:

```toml
[security.webhooks.replay]
backend = "memory"
allow_memory_in_production = true
```

Multi-replica production deployments should use Redis:

```toml
[security.webhooks.replay]
backend = "redis"

[security.webhooks.replay.redis]
url = "redis://redis:6379/0"
key_prefix = "myapp:webhooks:replay"
```

The Redis backend uses an atomic `SET NX EX` write, so every replica shares the
same delivery-ID claim and Redis expires claims after the configured replay
window. Compile `autumn-web` with the `redis` feature when selecting this
backend.

## Logging posture

Do not log signing secrets or full raw payloads. Log the provider, endpoint,
delivery ID, event type, status, and request ID. If you need payload diagnostics,
log a bounded hash or a redacted subset after verification.

## Synchronous versus background work

Do only quick validation and idempotent acceptance in the webhook handler. For
slow work such as sending mail, syncing a repository, or updating a billing
projection, enqueue a `#[job]` after verification and return promptly. Jobs are
recommended for follow-up processing, but they are not required to accept a
signed webhook.

See `examples/reddit-clone/src/routes/webhooks.rs` for runnable fixture tests covering valid,
tampered-body, stale-timestamp, bad-signature, and duplicate-delivery cases.
