# Operator Alerts

Autumn already knows when your app is in trouble: a background job gets
dead-lettered, a health indicator goes `Down`, the 5xx rate spikes, or a
framework-scheduled task fails. **Operator alerts** connect those built-in
failure signals to the delivery channels your app already has — the configured
mailer and the signed outbound webhook — so you find out **without writing any
application code**.

Provide an operator email and/or a webhook URL under `[alerts]` and you are
done: every built-in condition is delivered, deduplicated, with a recovery
notice when it clears.

> Alerts reuse your existing mailer and outbound-webhook machinery — **no new
> external dependency**. Delivery is best-effort and off the request path: if a
> channel is unreachable the app keeps serving, the failure is logged, and **no
> latency is added to any request**.

---

## Quick start

```toml
# autumn.toml
[alerts]
email = "oncall@example.com"
webhook_url = "https://alerts.example.com/hooks/autumn"
```

That's it. With a destination configured, a scaffolded app delivers an alert for
every built-in condition below. Prefer environment variables for secrets and
per-environment destinations:

```bash
AUTUMN_ALERTS__EMAIL=oncall@example.com
AUTUMN_ALERTS__WEBHOOK_URL=https://alerts.example.com/hooks/autumn
AUTUMN_ALERTS__WEBHOOK_SECRET=…       # HMAC signing secret for the webhook
```

`autumn doctor` warns (in production mode) when no destination is configured, so
a deploy never runs silently blind to its own failures.

---

## Built-in conditions, defaults, and how to tune each

Every condition is on as soon as a destination is configured. Each carries a
**stable dedup key**, a **severity** (`critical` on trigger, `recovery` on
resolve), a **timestamp**, the **host/replica** it fired on, and a **where to
look next** pointer.

| Condition | Fires when | Where to look | Tuning knob (default) |
|-----------|------------|---------------|-----------------------|
| **Dead-lettered job** | a background job exhausts its retries and is dead-lettered | `/actuator/jobs` | always on |
| **Health indicator Down** | a registered health indicator reports `Down` continuously past a grace period | `/actuator/health` | `health_grace_secs` (`60`) |
| **High 5xx rate** | the rolling 5xx rate crosses a threshold | `/actuator/metrics` | `error_rate_threshold` (`0.05`), `error_rate_min_requests` (`20`) |
| **Scheduled-task failure** | a framework-scheduled task (cron or fixed-delay — e.g. backup, cert-renewal) returns an error | `/actuator/tasks` | always on |

The 5xx-rate and health conditions are evaluated on a **background tick**
(`eval_interval_secs`, default `30`) — never on the request path — so they add
no request latency (AC #6). The 5xx rate is measured over the requests seen
since the previous tick; it is only evaluated once at least
`error_rate_min_requests` requests have been seen in that window, so a couple of
errors during a quiet period never trip a false alarm.

### Full `[alerts]` reference

```toml
[alerts]
enabled = true                 # master switch (default true)
email = "oncall@example.com"   # operator email destination
webhook_url = "https://…"      # signed webhook destination
webhook_secret = "…"           # HMAC secret (prefer AUTUMN_ALERTS__WEBHOOK_SECRET)

# Deduplication
dedup_window_secs = 900        # at most one notice per condition per 15 min

# Condition (b): health indicator Down
health_grace_secs = 60         # indicator must stay Down this long before alerting

# Condition (c): 5xx rate
error_rate_threshold = 0.05    # 5% of sampled requests are 5xx
error_rate_min_requests = 20   # ignore the rate below this sample size

# Background evaluation
eval_interval_secs = 30        # cadence for the health + 5xx-rate conditions
```

Every key above is also settable via `AUTUMN_ALERTS__<KEY>` (e.g.
`AUTUMN_ALERTS__ERROR_RATE_THRESHOLD=0.02`).

---

## Deduplication and recovery

A sustained or repeating condition does **not** produce one notification per
occurrence. Autumn bounds it to **at most one notification per condition per
dedup window** (`dedup_window_secs`, default 15 minutes). While the condition
keeps firing it re-notifies at most once per window as a reminder. When a
previously-alerted condition clears, **exactly one recovery notification** is
sent (severity `recovery`, event `resolve`), carrying the same stable dedup key
as its trigger so an incident manager can auto-resolve the correlated alert.

### Silencing a condition

- **Silence everything:** set `enabled = false`. This is the master off switch
  and silences **all** alerts — not just the built-in mail and webhook channels
  but every custom [`AlertChannel`] registered with `with_alert_channel` too. No
  channels are installed, the background evaluation loop is never started, and
  the `notify_*` hooks become no-ops, so nothing is delivered anywhere.
  (Removing the destination only silences the built-in channels.)
- **Quiet the 5xx alert:** raise `error_rate_threshold` (e.g. `0.2`) or
  `error_rate_min_requests`.
- **Tolerate flapping dependencies:** raise `health_grace_secs` so a brief blip
  never alerts.
- **Reduce reminder volume:** raise `dedup_window_secs`.

---

## What an alert contains

Each alert states **what** failed, **when**, on **which host/replica**, and
**where to look next** (an actuator endpoint or a log correlation id) — AC #4.
The webhook payload is JSON:

```json
{
  "dedup_key": "dead_lettered_job:reporting_job",
  "condition": "dead_lettered_job",
  "severity": "critical",
  "event": "trigger",
  "title": "Job 'reporting_job' was dead-lettered",
  "summary": "Background job 'reporting_job' exhausted its retries …",
  "timestamp": "2026-07-10T12:00:00Z",
  "host": "web-7c9f",
  "where_to_look": "/actuator/jobs",
  "details": { "job": "reporting_job", "error": "connection refused" }
}
```

The webhook is signed exactly like Autumn's outbound webhooks: an
`Autumn-Signature: t=<unix>,v1=<hmac-sha256>` header over `"<t>.<body>"` using
`webhook_secret`. Verify it the same way you verify any Autumn outbound webhook.

Email alerts are delivered through your configured mailer with the
bounce/complaint **suppression list bypassed** — operator alerts are
security-class and must never be silently dropped.

> Email alerts require the `mail` feature. If your binary is built without it, an
> email-only `[alerts]` destination delivers nothing — Autumn logs a startup
> `warn` in that case. Enable the `mail` feature or configure a `webhook_url`
> destination instead.

The host/replica identity is read from `AUTUMN_REPLICA_ID`, falling back to
`HOSTNAME`.

---

## Adding your own destination (PagerDuty, Slack, Discord, …)

Delivery is a trait. Implement [`AlertChannel`] and register it — the built-in
mail/webhook channels stay active alongside yours. This is the extension seam
for additional transports; the framework core never changes.

> Custom channels are still governed by the master switch: with
> `enabled = false` your channel is not installed and receives nothing, exactly
> like the built-in ones.

```rust,no_run
use autumn_web::alerts::{Alert, AlertChannel, AlertDeliveryError, AlertDeliveryFuture};

struct PagerDuty {
    routing_key: String,
}

impl AlertChannel for PagerDuty {
    fn name(&self) -> &'static str { "pagerduty" }

    fn deliver<'a>(&'a self, alert: &'a Alert) -> AlertDeliveryFuture<'a> {
        Box::pin(async move {
            // PagerDuty correlates incidents on `alert.dedup_key`; map
            // `alert.severity` and `alert.event` onto its trigger/resolve API.
            let _ = (&self.routing_key, &alert.dedup_key, alert.severity, alert.event);
            Ok::<(), AlertDeliveryError>(())
        })
    }
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .with_alert_channel(PagerDuty { routing_key: "…".into() })
        .run()
        .await;
}
```

Because every alert carries a stable `dedup_key`, a `severity` class, and a
`trigger`/`resolve` event, external incident managers can correlate and
auto-resolve alerts without any per-condition glue.

[`AlertChannel`]: https://docs.rs/autumn-web/latest/autumn_web/alerts/trait.AlertChannel.html
