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
webhook_secret = "…"   # required with webhook_url; prefer the env var below
```

That's it. With a destination configured, a scaffolded app delivers an alert for
every built-in condition below. A `webhook_url` requires a `webhook_secret`
(alerts are always signed). Prefer environment variables for secrets and
per-environment destinations:

```bash
AUTUMN_ALERTS__EMAIL=oncall@example.com
AUTUMN_ALERTS__WEBHOOK_URL=https://alerts.example.com/hooks/autumn
AUTUMN_ALERTS__WEBHOOK_SECRET=…       # HMAC signing secret for the webhook
```

`autumn doctor` warns (in production mode) when no destination is configured, so
a deploy never runs silently blind to its own failures. An `email` destination
only counts when a usable `[mail] transport` is configured: the mailer defaults
to `disabled` outside dev, and a disabled mailer installs no email alert channel
(it silently drops mail), so doctor warns for an email paired with a disabled
transport just as it does for a missing destination. Set `[mail] transport` to a
real backend (`smtp`/`log`/`file`) or use a signed webhook. Email alerts also
need a sender address: the alert mail carries no per-message `from`, so it uses
the mailer default (`[mail] from`). With `transport = "smtp"` and no `[mail]
from`, SMTP delivery fails with "mail from address is required", so doctor warns
(in production) for an SMTP email destination with no `[mail] from` — set
`[mail] from` (or `AUTUMN_MAIL__FROM`). The `log` and `file` transports deliver
without a `from`, so they are not gated on it. Doctor also honours
the master switch: when `[alerts] enabled = false` (or
`AUTUMN_ALERTS__ENABLED=false`) in production it warns that no operator alerts
will be delivered even though a destination is configured, because the runtime
installs no alerter at all when alerting is disabled.

`autumn doctor` is a config-only checker: it reads `autumn.toml` (and the
`AUTUMN_ALERTS__*` environment) and cannot see alert channels registered in code
with `AppBuilder::with_alert_channel`. If your app installs a custom
[`AlertChannel`] that way, alerts are still delivered even though nothing appears
under `[alerts]`, so the "no alert destination" warning is expected.

To make that pass — instead of ignoring the warning — declare the
code-registered channel so `autumn doctor --strict` succeeds:

```toml
[alerts]
custom_channel = true   # or AUTUMN_ALERTS__CUSTOM_CHANNEL=true
```

`custom_channel = true` tells doctor you register an alert channel in code via
`AppBuilder::with_alert_channel`, suppressing the no-destination warning so a
valid code-only deploy is not blocked by `--strict`. It is a doctor-only
declaration: the runtime installs code-registered channels regardless of this
flag, and it does **not** mask a *broken configured* destination — a malformed
`[alerts] email`, an SMTP email with no `[mail] from`, a disabled mail transport,
or a non-absolute `webhook_url` still warns. It suppresses only the pure "no
destination configured" case.

---

## Built-in conditions, defaults, and how to tune each

Every condition is on as soon as a destination is configured. Each carries a
**stable dedup key**, a **severity** (`critical` on trigger, `recovery` on
resolve), a **timestamp**, the **host/replica** it fired on, and a **where to
look next** pointer.

| Condition | Fires when | Where to look | Tuning knob (default) |
|-----------|------------|---------------|-----------------------|
| **Dead-lettered job** | a background job exhausts its retries and is dead-lettered | `/actuator/jobs` † | always on |
| **Health indicator Down** | a registered health indicator reports `Down` continuously past a grace period | `/actuator/health` | `health_grace_secs` (`60`) |
| **High 5xx rate** | the rolling 5xx rate crosses a threshold | `/actuator/metrics` | `error_rate_threshold` (`0.05`), `error_rate_min_requests` (`20`) |
| **Scheduled-task failure** | a framework-scheduled task (cron or fixed-delay — e.g. backup, cert-renewal) returns an error | `/actuator/tasks` † | always on |

The "where to look" paths above assume the default actuator prefix. If you
change `[actuator] prefix` (or set `AUTUMN_ACTUATOR__PREFIX`), each alert's
`where_to_look` is rebuilt from the configured prefix — e.g. with
`prefix = "/_ops"` a dead-lettered-job alert points at `/_ops/jobs` — so it
always references the endpoint you actually mounted rather than a `/actuator/*`
404.

**† `/actuator/jobs` and `/actuator/tasks` require `[actuator] sensitive = true`.**
Those two endpoints are mounted only when the sensitive actuator surface is
enabled, and `[actuator] sensitive` defaults to `false` (off in production). When
it is off, the dead-lettered-job and scheduled-task-failure alerts do **not** link
those endpoints (they would 404); instead they point at the always-mounted
`/actuator/health` and note that the richer `/jobs` (resp. `/tasks`) endpoint
becomes available once you set `[actuator] sensitive = true`. `/actuator/health`
and `/actuator/metrics` are always mounted, so the health and 5xx-rate alerts link
them unconditionally.

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
webhook_secret = "…"           # REQUIRED with webhook_url; alerts are always
                               # signed (prefer AUTUMN_ALERTS__WEBHOOK_SECRET)

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

For the **health-indicator Down** condition, "clears" means the indicator
reports a genuinely healthy status again (`UP`, or `UNKNOWN` — both of which
`/actuator/health` treats as healthy). A `Down` indicator that later reports
`OUT_OF_SERVICE` is **not** a recovery: the service is still non-healthy, so the
alert stays active and no false recovery is emitted until the indicator is
actually healthy.

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

## Limitations

Deduplication is **process-local**: each replica keeps its own in-memory record
of which conditions are currently firing. For the 5xx-rate and health-indicator
conditions this is exactly right — each replica evaluates its own metrics and its
alerts carry a host-scoped dedup key, so incidents stay separate per replica.

There is one known gap for **scheduled-task recovery on multi-replica fleets**.
A scheduled task is lease-coordinated across the fleet, so it can fail on one
replica and later succeed on another after a leader handoff. Because the failure
and the success were observed by different replicas — and the recovery is gated
by the replica-local record of the failure — the replica that runs the success
has no outstanding failure to clear, so the recovery notice is skipped and the
original failure alert may linger until it ages out. **Single-VPS deployments
(the common case) are unaffected**, since there is only one replica; only
multi-replica fleets can hit this after a leader handoff. Cross-app or
fleet-level alert aggregation and shared active-alert state are tracked as a
follow-up (#1630) and are out of scope here.

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

`where_to_look` uses your configured actuator prefix, so under a custom
`[actuator] prefix` (e.g. `/_ops`) it reads `/_ops/jobs` instead. The example
above shows the value with `[actuator] sensitive = true`; with the default
`sensitive = false` this dead-lettered-job alert instead reads
`/actuator/health (/actuator/jobs requires [actuator] sensitive = true)`, because
`/actuator/jobs` is not mounted (see the table note above).

Alert webhooks are **always signed**, exactly like Autumn's outbound webhooks:
an `Autumn-Signature: t=<unix>,v1=<hmac-sha256>` header over `"<t>.<body>"` using
`webhook_secret`. Verify it the same way you verify any Autumn outbound webhook.

Because alerts are always signed, **`webhook_secret` is required** whenever a
`webhook_url` is configured. Set it in `[alerts]` or, preferably, via the
`AUTUMN_ALERTS__WEBHOOK_SECRET` environment variable (which overrides the file).
If a `webhook_url` is configured but no non-empty `webhook_secret` resolves,
Autumn logs a startup `warn` and **does not register the webhook channel** —
it never sends unsigned requests that your receiver would reject.

`webhook_url` must be an **absolute `http(s)` URL** — it has to start with
`http://` or `https://` and include a host (surrounding whitespace, common when
the value comes from a copied env var, is trimmed automatically). A relative or
malformed value could never be dispatched, so Autumn logs a startup `warn` and
**does not register the webhook channel** rather than installing one that looks
configured but fails every delivery.

Email alerts are delivered through your configured mailer with the
bounce/complaint **suppression list bypassed** — operator alerts are
security-class and must never be silently dropped.

Just as with webhooks, Autumn refuses to register a mail alert channel that
could never deliver. It logs a startup `warn` and **does not register the mail
channel** when the `[mail] transport` is `disabled` (it silently drops mail),
when `[alerts] email` is not a valid address (lettre parses the recipient only
when sending, so a malformed value like `not-an-address` or a `mailto:` URI
would fail every delivery with an invalid-address error), or when the transport
is `smtp` with no `[mail] from` (the alert mail carries no per-message `from`, so
SMTP send fails with "mail from address is required"). The `log` and `file`
transports deliver without a `from`, so they are never gated on it. These runtime
skips mirror the `autumn doctor` warnings above, so doctor and the running app
agree on which email destinations are usable.

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
