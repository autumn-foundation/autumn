# Operator Alerts

Autumn already knows when your app is in trouble: a background job gets
dead-lettered, a health indicator goes unhealthy, the 5xx rate spikes, or a
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
`[mail] from` (or `AUTUMN_MAIL__FROM`). The `from` must also be a *valid* mailbox:
the runtime parses `[mail] from` at boot and refuses to start when it is set to an
unparsable value like `not-a-mailbox`, so doctor warns (in production) for a
present-but-invalid `[mail] from` on an SMTP transport — naming the offending
value — to catch it before deploy rather than via a boot crash. Display-name
senders such as `Ops <ops@example.com>` are accepted (they parse fine). The `log`
and `file` transports deliver without a `from`, so they are not gated on it.
Doctor also honours
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
| **Health indicator down** | a registered health indicator reports a non-healthy status (`DOWN` or `OUT_OF_SERVICE`) continuously past a grace period | `/actuator/health` | `health_grace_secs` (`60`) |
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

`error_rate_threshold` is a **fraction of sampled requests in `(0, 1]`** (it is
compared against `errors / requests`), so `0.05` means "5% of sampled requests
returned 5xx" and `1.0` means "100%". A value outside `(0, 1]` — non-finite
(`nan`/`inf`), zero or negative, or greater than `1` — silently breaks the alert:
a value `> 1` (or `nan`) can never be reached, so the 5xx alert would **never**
fire, while a value `<= 0` would fire on a window with zero errors. Autumn is
fail-safe here: at startup an invalid `error_rate_threshold` is **ignored and
falls back to the default `0.05`** (logged with a `warn`), so 5xx alerting keeps
working. `autumn doctor` also flags an invalid value in production so you can fix
the config rather than unknowingly run on the default.

### Full `[alerts]` reference

```toml
[alerts]
enabled = true                 # master switch (default true)
email = "oncall@example.com"   # operator email destination
webhook_url = "https://…"      # signed webhook destination
webhook_secret = "…"           # REQUIRED with webhook_url; alerts are always
                               # signed (prefer AUTUMN_ALERTS__WEBHOOK_SECRET)

# Native provider transports (see "Native alerting transports" below)
pagerduty_routing_key = "…"    # PagerDuty Events API v2 routing key
                               # (prefer AUTUMN_ALERTS__PAGERDUTY_ROUTING_KEY)
pagerduty_url = "https://events.pagerduty.com/v2/enqueue"  # override for a
                               # PagerDuty-Events-compatible endpoint (optional)
pagerduty_severities = "all"   # "all" (default) or "critical"
slack_webhook_url = "https://hooks.slack.com/services/…"   # Slack incoming webhook
slack_severities = "all"       # "all" (default) or "critical"
discord_webhook_url = "https://discord.com/api/webhooks/…/slack"  # Discord (Slack-
                               # compatible endpoint — append /slack)
discord_severities = "all"     # "all" (default) or "critical"

# Deduplication
dedup_window_secs = 900        # at most one notice per condition per 15 min

# Condition (b): health indicator down
health_grace_secs = 60         # indicator must stay non-healthy this long before alerting

# Condition (c): 5xx rate
error_rate_threshold = 0.05    # fraction in (0, 1]; 0.05 = 5% of sampled requests
                               # are 5xx. An invalid value (non-finite, <= 0, or
                               # > 1) falls back to the default 0.05 at startup.
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

For the **health-indicator down** condition, the alert **fires** whenever an
indicator reports any non-healthy status — `DOWN` *or* `OUT_OF_SERVICE` — past
the grace period. (An indicator that jumps straight to `OUT_OF_SERVICE` without
first going `DOWN` is alerted just the same; `/actuator/health` returns a non-200
for either.) The alert names the actual status it observed. "Clears" means the
indicator reports a genuinely healthy status again (`UP`, or `UNKNOWN` — both of
which `/actuator/health` treats as healthy). An already-alerted indicator that
transitions from `DOWN` to `OUT_OF_SERVICE` is **not** a recovery: the service is
still non-healthy, so the alert stays active and no false recovery is emitted
until the indicator is actually healthy.

### Dead-lettered jobs: bounded per job type

The **dead-lettered-job** condition is deduplicated **per job type** — its dedup
key is `dead_lettered_job:{job_name}`, scoped to the job's name, not the
individual job instance. So when many instances of the *same* job type
dead-letter within the dedup window (a mass failure — a dependency outage
dead-lettering every `reporting_job` in the queue), you receive a **bounded
number of alerts** (at most one per job type per window) rather than one alert
per failed job. This deliberately protects a single-VPS operator from being
flooded during an incident.

So the bounded alert never hides *which* job failed, it names a concrete,
representative failed instance: the specific job's id appears in the alert title
and summary (`Job 'reporting_job' (id job-abc-123) was dead-lettered: …`) and in
the structured `job_id` detail field. The **full set** of dead-lettered jobs —
every instance, not just the one named in the alert — is always visible at the
`/actuator/jobs` endpoint (the alert's "where to look next" pointer), so start
from the named id and consult `/actuator/jobs` for the complete list.

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
fleet-level alert aggregation and shared active-alert state remain a separate
future follow-up (the native PagerDuty/Slack/Discord transports do not change
this process-local behaviour) and are out of scope here.

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
  "title": "Job 'reporting_job' (id job-abc-123) was dead-lettered",
  "summary": "Background job 'reporting_job' (id job-abc-123) exhausted its retries …",
  "timestamp": "2026-07-10T12:00:00Z",
  "host": "web-7c9f",
  "where_to_look": "/actuator/jobs",
  "details": { "job": "reporting_job", "job_id": "job-abc-123", "error": "connection refused" }
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

## Native alerting transports (PagerDuty, Slack, Discord)

Autumn delivers its built-in alerts natively to the paging and chat tools most
teams already run — **config-only, zero app code**. Add a routing key or a
webhook URL under `[alerts]` and the provider is wired up, correlated, and
fail-safe, alongside any email/webhook destination.

All three use the SSRF-hardened outbound HTTP client and deliver off the request
path exactly like the built-in webhook: a provider outage or a rejected event
never affects request serving and adds no request latency; failures are logged.

### PagerDuty (and PagerDuty-Events-compatible pagers)

```toml
[alerts]
pagerduty_routing_key = "…"    # prefer AUTUMN_ALERTS__PAGERDUTY_ROUTING_KEY
```

Each alert is delivered as a **PagerDuty Events API v2** event correlated on the
alert's stable `dedup_key`, so a repeating condition folds into a **single
incident** instead of a page storm. When the condition clears, Autumn sends a
`resolve` event with the same `dedup_key` and the incident **auto-resolves**. A
firing alert maps to PagerDuty severity `critical`; the event carries the
summary, source (host/replica), timestamp, condition component, and the alert's
`where_to_look` plus structured details in `custom_details`.

Point `pagerduty_url` at a PagerDuty-Events-compatible enqueue endpoint offered
by another paging service to reuse this same channel:

```toml
[alerts]
pagerduty_url = "https://events.eu.pagerduty.com/v2/enqueue"
```

### Slack (and Discord via its Slack-compatible endpoint)

```toml
[alerts]
slack_webhook_url  = "https://hooks.slack.com/services/T…/B…/…"
discord_webhook_url = "https://discord.com/api/webhooks/…/…/slack"   # append /slack
```

The Slack channel posts a human-readable message carrying the required fields —
what failed, when, on which host/replica, and where to look next — plus any
structured details. **Discord** is supported through its **Slack-compatible
webhook endpoint**: append `/slack` to a Discord webhook URL and Autumn sends
the exact same payload dialect, so one message format covers both chat tools.

Both webhook URLs must be **absolute `https` URLs** (Slack and Discord only
expose `https` endpoints). A relative or non-`https` value is skipped with a
startup `warn`, and `autumn doctor` flags it in production.

### Multiple destinations fan out

Every configured destination receives the same alert. Set a PagerDuty routing
key **and** a Slack webhook and a critical condition pages the on-call phone and
posts to the incident channel simultaneously; add the generic webhook and email
and all four fire.

### Per-channel severity routing

Each native destination declares which severities it receives via its
`*_severities` key — `"all"` (the default) or `"critical"`:

```toml
[alerts]
pagerduty_routing_key = "…"
pagerduty_severities  = "all"       # page on failure AND auto-resolve
slack_webhook_url     = "https://hooks.slack.com/services/…"
slack_severities      = "critical"  # notify chat on failure, stay quiet on recovery
```

A channel set to `"critical"` receives only firing (`critical`) alerts;
recovery/informational (`recovery`) alerts are **verifiably not delivered** to
it. The recommended pattern is to page a human on critical conditions while a
chat channel merely notifies. Leave **PagerDuty on `"all"`** (the default) so the
`resolve` event reaches it and incidents auto-resolve — setting it to
`"critical"` suppresses the resolve and incidents stay open until they age out.

Severity routing applies only to Autumn's built-in transports; a custom
[`AlertChannel`] you register in code receives every severity unless it overrides
`accepts_severity`.

### Verify wiring before an incident: `autumn alert test`

Fire a synthetic alert through every configured outbound-HTTP channel and see
per-channel success or an actionable error — before you are relying on it:

```bash
autumn alert test                 # every configured channel
autumn alert test --channel slack # just one (pagerduty | slack | discord | webhook)
```

It reads your effective `[alerts]` config (env vars and profiles honoured) and
uses the same channel implementations the server installs, so a green run proves
the real delivery path. (Email is validated by `autumn doctor` and a real send,
since it needs a configured mailer.)

### Acknowledge / resolve stance

This is a **one-directional** integration: Autumn → provider. Acknowledging or
resolving an incident **in the provider** does not silence Autumn's re-alerts —
Autumn's own dedup / re-alert windows (`dedup_window_secs`) bound notification
volume, and an Autumn-side recovery is what emits the `resolve` event. Inbound
ack/resolve synchronization (provider → Autumn silencing) is out of scope.

### Unsupported providers: the generic webhook fallback

A provider without native support (Opsgenie's own API, Microsoft Teams, Telegram,
SMS, …) is served either by its PagerDuty-Events- or Slack-compatible endpoint
where offered, or by the **generic signed webhook** (`webhook_url` +
`webhook_secret`, documented above) pointed at a small translation shim, or by a
custom [`AlertChannel`] in code (next section).

---

## Adding your own destination (a custom `AlertChannel`)

PagerDuty, Slack, and Discord are built in (above); this seam is for **any other
sink**. Delivery is a trait: implement [`AlertChannel`] and register it — the
built-in channels stay active alongside yours. The framework core never changes.

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
