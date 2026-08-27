# Staged and Zero-Downtime Deploys

This guide covers the deploy strategies that Autumn supports out of the box and
explains how the framework's probe contracts, drain lifecycle, and maintenance
mode fit together to give you a safe rollout in each case.

**Strategies covered:**

| Strategy | When to use | Framework support |
|---|---|---|
| Rolling | Standard incremental rollout | Full — probes + drain handle it automatically |
| Blue/green | Instant cutover with easy rollback | Full — probe drain + LB switch |
| Canary | Gradual traffic shift with automated promotion | Framework primitives — version-labelled metrics, a canary-route extractor, and a controller-driven rollback signal (see [Canary deploys](#canary-deploys)) |
| Shadow / differential | Proving the candidate answers identically before cutover | Full — in-process traffic mirroring, response differ, and `/actuator/shadow` (see [Shadow deploys](#shadow-differential-deploys)) |
| Rolling across your own VPS fleet | Several app servers you own, behind your own load balancer, with no orchestrator | Full — `autumn deploy up` with `[deploy] hosts` rolls the hosts one at a time (per-host blue/green, migrations exactly once, halt-and-roll-back on failure); see the [fleet deploys guide](fleet-deploys.md) |

---

## Rolling deploys

A rolling deploy replaces replicas one at a time. The orchestrator (Fly.io,
Kubernetes, Docker Swarm) starts a new replica, waits for it to pass its
readiness check, then terminates an old one. Traffic flows through the healthy
mix of old and new at all times — no downtime window, no manual intervention.

Autumn's probe and drain contracts are designed around this pattern.

### How it works

```
Old replica A  [live] ──────────────────── SIGTERM ─→ drain ─→ exit
Old replica B  [live] ──────────────────────────────────── SIGTERM ─→ drain ─→ exit
New replica C          [starting] ─→ [ready] ─→ [live, serving traffic]
New replica D                             [starting] ─→ [ready] ─→ [live, serving traffic]
```

The key invariant: **`/ready` flips to 503 before the listener closes.** This
gives the load balancer time to deregister the old replica before it stops
accepting connections. No request hits a closing socket.

Concretely, on SIGTERM:

1. `/ready` → 503 immediately (load balancer stops routing new requests here).
2. `prestop_grace_secs` (default 5 s) — time for the load balancer to drain its
   connection pool to this replica.
3. The TCP listener closes.
4. In-flight requests complete (up to `shutdown_timeout_secs`, default 30 s).
5. App hooks, telemetry flush, DB pool close, process exits.

A new replica is only promoted to live after `/ready` returns 200 — which
Autumn gates until the DB connection pool is established and any startup probes
have passed.

### Config knobs

```toml
# autumn.toml
[server]
prestop_grace_secs   = 5    # wait for LB to drain before closing listener
shutdown_timeout_secs = 30  # max time for in-flight requests to complete
```

For Fly.io, `kill_timeout` in `fly.toml` must be at least
`prestop_grace_secs + shutdown_timeout_secs + buffer`:

```toml
# fly.toml
[deploy]
  kill_timeout = 45   # 5 + 30 + 10 s buffer
```

### Migration safety

Schema migrations must run **before** new replicas start. An incompatible
schema during the rollout causes errors on old replicas. The safe sequence:

```bash
# 1. Run migrations (schema changes land before any replica restarts)
autumn migrate

# 2. Deploy new replicas (rolling, one at a time)
fly deploy
```

For destructive schema changes (column drops, renames), use the
expand/contract pattern: add the new column in one deploy, migrate data,
drop the old column in a later deploy when no live code references it anymore.
`autumn migrate check` classifies SQL statements by rolling-deploy risk before
you run them — see [deployment.md](deployment.md).

### Runnable repro

```bash
# Watch /ready flip during a local graceful shutdown
curl -s http://localhost:3000/ready   # → 200
kill -TERM $(pgrep myapp)
curl -s http://localhost:3000/ready   # → 503 (within prestop_grace_secs window)
# In-flight requests complete; process exits after shutdown_timeout_secs
```

---

## Blue/green deploys

A blue/green deploy keeps two complete environments alive simultaneously — the
current live environment (blue) and the new environment (green). Traffic is
switched atomically at the load balancer. Rollback is a second switch back to
blue, with no re-deploy.

This is the right choice when:

- You want instant rollback without re-deploying the old image.
- Your migration is not backward-compatible and you need to keep the old schema
  live until you are confident the new version is healthy.
- You are switching a major dependency (Postgres version, Redis version) and
  want the old stack available for comparison.

### Architecture

```
                         ┌─────────────────────────────┐
Internet ──→ LB ──────→  │  Blue  (current, 100% traffic)│
                         └─────────────────────────────┘
                         ┌─────────────────────────────┐
                         │  Green (new, 0% traffic)     │  ← warming up
                         └─────────────────────────────┘
```

After the switch:

```
                         ┌─────────────────────────────┐
                         │  Blue  (old, 0% traffic)     │  ← idle, available for rollback
                         └─────────────────────────────┘
                         ┌─────────────────────────────┐
Internet ──→ LB ──────→  │  Green (new, 100% traffic)   │
                         └─────────────────────────────┘
```

### Procedure

**Step 1 — Stand up the green environment**

Deploy the new image to a separate set of replicas. Do not send traffic yet.

```bash
# Fly example: deploy to a separate app
fly deploy --app myapp-green

# Kubernetes example: apply to a second Deployment
kubectl apply -f deploy/green.yaml
```

**Step 2 — Warm up and verify green**

Green replicas must pass all three probes before you switch traffic:

```bash
# Startup probe — passes once once the binary is listening
curl -f https://myapp-green.internal/startup

# Liveness probe — passes when the process is healthy
curl -f https://myapp-green.internal/live

# Readiness probe — passes when DB pool is up and ready to serve
curl -f https://myapp-green.internal/ready
```

Run your smoke suite against the green environment directly (before any traffic
switch). Autumn's `/actuator/health` returns the DB pool status and replica lag
so you can confirm the green environment has a working database connection:

```bash
curl -s https://myapp-green.internal/actuator/health | jq .
```

**Step 3 — Run migrations (if any)**

Migrations must target the same database as both environments. Run them before
switching traffic so green replicas start with the new schema already in place:

```bash
autumn migrate
```

If the migration is destructive and the old blue code cannot run against the new
schema, put blue into maintenance mode while migrating:

```bash
autumn maintenance on --message "Upgrading — back in a few minutes."
autumn migrate
```

See [maintenance-mode.md](maintenance-mode.md) for the full runbook.

**Step 4 — Switch traffic**

Redirect 100% of traffic from blue to green at the load balancer:

```bash
# Fly example: update the DNS / Fly anycast IP to point at green
fly ips assign --app myapp-green $(fly ips list --app myapp --json | jq -r '.[0].Address')

# Kubernetes example: flip the Service selector
kubectl patch service myapp -p '{"spec":{"selector":{"version":"green"}}}'
```

**Step 5 — Drain blue**

Blue replicas are still running but no longer receiving traffic. Leave them
running for a rollback window (typically 10–30 minutes), then shut them down:

```bash
# Fly example
fly scale count 0 --app myapp-blue

# Kubernetes example
kubectl scale deployment myapp-blue --replicas=0
```

Because blue's `/ready` endpoint has already been deregistered from the LB,
you can stop the blue processes immediately with no drain needed — no traffic is
flowing to them.

**Rollback**

If green is unhealthy, switch the LB back to blue before stopping it. Blue
never lost its database connection and its code was never changed — it is live
immediately:

```bash
kubectl patch service myapp -p '{"spec":{"selector":{"version":"blue"}}}'
```

Then tear down green, diagnose the issue, and re-deploy when ready.

### Key points

- Autumn's probe contracts give you a deterministic signal for when green is
  ready (`/ready` → 200) and when blue has finished draining (process exits
  cleanly after SIGTERM).
- **Autumn never distributes traffic between hosts.** How much traffic each
  machine receives is a load-balancer concern — Autumn gives you the health
  signals and you (or your platform) decide when to pull the lever. What Autumn
  *does* manage is the blue/green switch **within** a single deploy-managed host:
  `autumn deploy up` stands the candidate up on that host's idle slot,
  health-gates it, and flips the host's own kamal-proxy atomically. Across
  several hosts (`[deploy] hosts`) it repeats that host by host — still without
  touching your load balancer's membership. See the
  [fleet deploys guide](fleet-deploys.md).
- Use `autumn doctor` on the green environment before switching traffic to catch
  misconfigured secrets, missing database URLs, or active maintenance flags:

  ```bash
  autumn doctor
  ```

---

## Canary deploys

A canary deploy routes a small percentage of traffic to a new version while the
rest continues to hit the old version. Automated metrics (error rate, latency
p99) gate promotion — if the canary looks healthy, traffic weight shifts
gradually to 100%; if it degrades, the canary is rolled back automatically.

**The load-balancer traffic split itself stays a platform concern** (Fly.io
machine weights, Kubernetes `TrafficPolicy`, Nginx upstream `weight`). What
Autumn provides is the set of framework primitives a canary controller drives:

| Primitive | What it gives the controller |
|---|---|
| **Deploy-version labelling** | Each replica resolves a `version` label from the environment so its metrics are attributable to the canary or stable cohort. |
| **Version-labelled metrics** | `autumn_http_requests_total`, `autumn_http_responses_total`, and `autumn_http_request_duration_seconds` carry a `version` label so a controller can compare cohorts. |
| **Canary-route identification** | A typed extractor exposes the load balancer's `X-Canary` routing decision to application code. |
| **Rollback signal** | A file-flag (or `autumn canary rollback`) tells a bad canary replica to drain `/ready → 503` and exit cleanly — no manual `SIGTERM`. |

### 1. Label the replica

Set one of these on the canary replica (no application code change required):

```bash
# Explicit label — wins over everything else. Use any string you like.
AUTUMN_DEPLOY_VERSION=canary

# …or the shorthand boolean (resolves to version="canary"):
AUTUMN_CANARY=true
```

Stable replicas leave both unset and report `version="stable"`. Autumn resolves
the label once at startup and logs it when the replica is the canary.

### 2. Compare cohorts via metrics

Every metric family on the `/actuator/prometheus` endpoint is tagged with the
replica's `version` label, so a controller scraping both cohorts can diff them:

```
autumn_http_requests_total{version="canary"} 412
autumn_http_responses_total{version="canary",status="5xx"} 3
autumn_http_responses_total{version="stable",status="5xx"} 0
autumn_http_request_duration_seconds{version="canary",quantile="0.99"} 1.2
autumn_http_request_duration_seconds{version="stable",quantile="0.99"} 0.21
```

A controller polls these between traffic-weight steps and decides whether to
keep shifting weight up or to roll back.

### 3. (Optional) React to canary routing in app code

If you opt specific users into the canary at the edge (the LB stamps
`X-Canary: true` on canary-bound requests), the `CanaryRoute` extractor lets a
handler see that decision without parsing headers by hand:

```rust
use autumn_web::canary::CanaryRoute;

async fn handler(canary: CanaryRoute) -> String {
    if canary.routed_to_canary {
        "served by the canary cohort".into()
    } else {
        "served by stable".into()
    }
}
```

The extractor never fails — a missing or non-truthy header means
`routed_to_canary == false`.

### 4. Roll back cleanly

When the controller decides the canary is unhealthy, it triggers a rollback. The
running replica notices within ~500 ms and runs the **same graceful-shutdown
sequence as `SIGTERM`**: `/ready → 503`, prestop grace, listener close,
in-flight drain, clean exit. The load balancer deregisters the replica as soon
as `/ready` flips, so no request hits a closing socket.

```bash
# From inside the canary replica (or a controller that can exec into it):
autumn canary rollback --reason "p99 latency exceeded" --by ci-controller

# Inspect / clear the signal:
autumn canary status
autumn canary promote   # clears the rollback flag (promotion of traffic is a platform step)
```

`autumn canary rollback` writes `tmp/autumn-canary-rollback.json`; the file-flag
protocol mirrors [maintenance mode](#how-it-works) so a controller that cannot
run the CLI can write the JSON directly. Because the flag lives in the replica's
working directory, target the specific canary container.

The flag is **sticky across restarts**: if a supervisor restarts the replica
while the flag is still present, it flips `/ready` to draining at startup and
drains again instead of rejoining the canary cohort. Clear it with
`autumn canary promote` (or scale the replica to zero) once traffic has moved.

> **Promotion** is a platform action: once metrics look good, shift the LB
> weight to 100% (or relabel the canary as the new stable) using your platform's
> mechanism, then `autumn canary promote` to clear any stale rollback flag.

### Worked example — Fly.io

Fly machines support per-machine traffic weight. Run a canary as an extra
machine in the same app with the canary label set:

```bash
# 1. Deploy the new image to a single extra machine, weighted at 5%.
fly deploy --image registry.fly.io/myapp:new \
  --strategy canary

# 2. Mark that machine as the canary cohort (env var, no code change).
fly machine update <canary-machine-id> --env AUTUMN_CANARY=true

# 3. Scrape both cohorts. Fly's metrics endpoint is wired to
#    /actuator/prometheus by the generated fly.toml [metrics] block.
#    A controller compares autumn_http_responses_total{version=...}.

# 4a. Healthy → raise weight, then promote (make new image the default).
fly machine update <canary-machine-id> --metadata fly_proxy_weight=50

# 4b. Unhealthy → roll the canary out cleanly, no SIGTERM:
fly ssh console --machine <canary-machine-id> -C "autumn canary rollback --reason 'p99 regression'"
# The machine drains (/ready → 503) and exits; Fly stops routing to it.
```

### Worked example — Kubernetes

Run the canary as a second Deployment behind the same Service, distinguished by
a `track` label, and let the controller (Argo Rollouts, Flagger, or a CI step)
drive the weight.

```yaml
# canary-deployment.yaml — the new version as a small replica set.
apiVersion: apps/v1
kind: Deployment
metadata:
  name: myapp-canary
spec:
  replicas: 1
  selector:
    matchLabels: { app: myapp, track: canary }
  template:
    metadata:
      labels: { app: myapp, track: canary }
    spec:
      containers:
        - name: myapp
          image: myapp:new
          env:
            - name: AUTUMN_CANARY
              value: "true"          # → version="canary" on this pod's metrics
          readinessProbe:
            httpGet: { path: /ready, port: 3000 }
          # Autumn flips /ready → 503 on rollback, so the Service endpoint
          # controller removes the pod before the listener closes.
```

```bash
# Controller loop (pseudo-steps):
# 1. Scrape canary vs stable:
#      autumn_http_responses_total{version="canary",status="5xx"}
#      autumn_http_request_duration_seconds{version="canary",quantile="0.99"}
# 2. Healthy → scale myapp-canary up / myapp (stable) down, then relabel.
# 3. Unhealthy → roll back cleanly without deleting the pod abruptly:
kubectl exec deploy/myapp-canary -- autumn canary rollback --reason "error budget burn"
#    The pod drains (/ready → 503) and exits 0; the ReplicaSet will not
#    receive traffic during the drain because the readiness gate is already
#    failing. Then `kubectl scale deploy/myapp-canary --replicas=0`.
```

In both examples Autumn never moves the traffic weight itself — it supplies the
version-labelled signals the controller gates on and the clean-drain rollback
the controller triggers.

---

## Shadow (differential) deploys

Every strategy above decides go/no-go from **aggregate cohort metrics** — error
rate, p99 — after routing **real** traffic to the new build. That catches a
build that falls over. It does not catch the build that returns `200 OK` with a
dropped JSON field, a reordered list, or an off-by-one total, because nothing
ever compares two responses to the same request.

Shadow mirroring does exactly that. Autumn samples live `GET`/`HEAD` traffic,
replays each sampled request against a candidate build you run alongside
production, and diffs the two responses. The candidate's output is consumed by
the differ and nothing else.

```text
                       ┌──────────────────┐
  client ──request──▶  │  live build      │ ──response──▶ client
                       └────────┬─────────┘        │
                                │ (copy)           │ (tee)
                       ┌────────▼─────────┐   ┌────▼─────┐
                       │ candidate build  │──▶│  differ  │──▶ /actuator/shadow
                       └──────────────────┘   └──────────┘        + metrics
```

### How it differs from canary

|  | Canary | Shadow |
|---|---|---|
| Does the new build serve real users? | **Yes** — a slice of live traffic | **No** — not one byte reaches a client |
| What is the signal? | Cohort metrics (error rate, p99) over a population | A per-request diff: *did these two builds answer the same?* |
| Catches a subtly-wrong `200`? | No — it is a `200` in both cohorts | **Yes** — that is the whole point |
| Blast radius of a bad candidate | Real users see it | None on the response path; the candidate's own side effects are yours to contain (see the warning below) |
| Needs a traffic split? | Yes, at the load balancer | No — the mirror is in-process |

The two compose. Run a shadow to prove behaviour equivalence, *then* canary to
prove the build holds up under real load.

### Turning it on

```toml
# autumn.toml
[shadow]
enabled          = true
target           = "http://127.0.0.1:9091"  # the candidate build you started
sample_rate      = 0.05    # fraction of ELIGIBLE traffic. Default: 1.0 — i.e.
                           # every eligible request. Start low.
routes           = ["/api/*"]  # empty (default) = every eligible route
timeout_ms       = 2000    # deadline per shadow request, and per mirrored
                           # primary response
max_in_flight    = 8       # concurrent mirrors; excess is dropped, never queued
max_body_bytes   = 262144  # larger responses are not compared, on either side
max_records      = 50      # divergences kept for the actuator
max_sample_bytes = 2048    # per recorded JSON sample, before truncation
```

Mirroring requires the `http-client` cargo feature (on by default). A build
without it logs a warning at startup and mirrors nothing; `/actuator/shadow`
then reports the configured target with `enabled: false`, so the state is
visible rather than silent.

Every key has an environment override (`AUTUMN_SHADOW__ENABLED`,
`AUTUMN_SHADOW__TARGET`, `AUTUMN_SHADOW__SAMPLE_RATE`, …), so a shadow run can
be switched on for one replica without redeploying the config.

Autumn does not build, start, or orchestrate the candidate — you run it (another
container, another port, another machine) and point `target` at it.

### What is guaranteed

- **The live request never waits on the mirror.** The shadow request is
  dispatched on a detached task before the primary handler finishes, and the
  primary response body is *teed* — frames reach the client as they are
  produced, with a copy accumulating on the side. A slow, erroring, or
  unreachable candidate resolves to a counter and nothing else.
- **Only idempotent methods are mirrored.** `GET` and `HEAD`, and the set is not
  configurable. Mirroring a `POST` would let the candidate's writes land for
  real; that needs effect virtualization, which is a follow-up.
- **Mirrored requests never touch primary state.** The mirror path performs no
  database, cache, mail, or job work of its own — it copies request bytes and
  compares response bytes.
- **The candidate's response never reaches a user.** It is read into a plain
  struct inside the detached task and dropped there; it is never a `Response`.
- **Mirroring cannot recurse.** Every mirrored request carries
  `X-Autumn-Shadow: 1`, and a request carrying that header is never mirrored
  again — so pointing a shadow target at the app itself costs one extra request,
  not an exponential storm. Your candidate build can read the same header to
  refuse writes.
- **Actuator and probe paths are never mirrored**, so load-balancer health
  checks do not drown the candidate.
- **Requests the live build refuses are not mirrored.** A response of `429` or
  `503` — the statuses maintenance mode, load shedding, the request deadline,
  and the rate limiter produce — skips the mirror entirely (counted as
  `refused`). The candidate is under none of those pressures, so it would answer
  normally and every request through a planned maintenance window would look
  like a status-class divergence. The trade is that a genuine handler-produced
  `429`/`503` divergence is not reported either.
- **A mirror cannot outlive its deadline.** `timeout_ms` bounds the shadow
  request *and* the wait for the mirrored primary response, so a client that
  stops reading — or a long-lived `text/event-stream` — cannot pin an
  `max_in_flight` slot indefinitely. Those are counted as `incomplete`.
- **Credentials never reach a proxy.** The mirroring client disables proxy
  autodetection, so `HTTP_PROXY`/`HTTPS_PROXY` in the environment cannot divert
  a mirrored request (carrying the end user's cookie) to a third party.
- **Forwarding headers are not replayed.** `X-Forwarded-*`, `Forwarded`, and
  `X-Real-IP` are stripped: this layer runs before the primary's trusted-proxy
  policy, so forwarding them would hand the candidate a client-spoofed value
  arriving from an address it *does* trust.
- **`Host` is preserved.** The candidate is *dialed* at `target`, but it sees
  the authority the live build accepted. Those are separate things, and only the
  address comes from `target`: a candidate that clones your
  `[security.trusted_hosts]` would otherwise reject every mirror with a `400`,
  and a subdomain-keyed multi-tenant app would resolve the wrong tenant. Behind
  a trusted proxy it is the **resolved** authority that travels — the one your
  `[security.trusted_proxies]` policy accepted — not the internal address in the
  raw `Host` header, and not the unvalidated `X-Forwarded-Host` itself.
- **Pages served from the static cache are mirrored too.** In an SSG/ISG build
  the static-first middleware answers matching `GET`/`HEAD` requests before the
  dynamic router runs; the mirror sits outside it, so a pre-rendered page the
  candidate generates differently is still compared.

### What is compared

Two things, and deliberately only two:

1. **Status class.** `2xx` vs `5xx` diverges. `200` vs `201` does not.
2. **Normalized body.** JSON is parsed and object keys sorted, so two builds
   serialising the same map in a different order agree. **Array order is
   preserved** — a reordered list *is* a divergence, because that is exactly the
   class of regression this exists to catch. Any other UTF-8 body has `\r\n`
   folded to `\n` and outer whitespace trimmed; anything that is not valid UTF-8
   is compared byte-for-byte.

   Which of those applies is decided by the **bytes**, never by `Content-Type`.
   Headers are outside the contract, so two builds returning an identical body
   must not diverge merely because one of them labelled it and the other did
   not.

Headers, latency, and fuzzy JSON tolerance are **not** compared. A response
whose body is larger than `max_body_bytes` is not compared at all (counted as
`skipped_oversize`) rather than partially buffered.

### Reading the results

```console
$ curl -s localhost:3000/actuator/shadow | jq
{
  "enabled": true,
  "target": "http://127.0.0.1:9091",
  "stats": {
    "mirrored": 4120, "compared": 4118, "matched": 4109, "diverged": 9,
    "shadow_errors": 2, "shadow_timeouts": 0,
    "dropped_at_capacity": 0, "skipped_oversize": 0
  },
  "divergences": [
    {
      "method": "GET",
      "target": "/api/orders?page=2",
      "route": "/api/orders",
      "occurrences": 9,
      "first_observed_at_ms": 1756300000000,
      "last_observed_at_ms": 1756300310000,
      "kind": "body",
      "primary_status": 200,
      "shadow_status": 200,
      "primary_body_kind": "json", "shadow_body_kind": "json",
      "primary_body_bytes": 118, "shadow_body_bytes": 104,
      "primary_digest": "9f2c…", "shadow_digest": "41ab…",
      "primary_sample": { "id": 7, "total": 42 },
      "shadow_sample": { "id": 7 },
      "fingerprint": "3d91a2f0c4e7b158"
    }
  ],
  "comparisons_by_route": [
    { "route": "/api/orders", "label": "diverged", "count": 9 },
    { "route": "/api/orders", "label": "match", "count": 4109 }
  ],
  "divergences_by_route": [
    { "route": "/api/orders", "label": "body", "count": 9 }
  ]
}
```

`stats` also carries `skipped_refused` and `primary_incomplete` (see the
outcomes below).

`/actuator/shadow` is a **sensitive** endpoint (`[actuator] sensitive = true`),
like `/actuator/tasks` — the samples are excerpts of real production responses.
A replica with mirroring off answers `{"enabled": false, …}` rather than `404`,
so you can tell "off here" from "not in this build".

Identical divergences collapse onto one record by `fingerprint`, a
content-addressed id derived only from the two responses. The same captured pair
always produces the same fingerprint, so a divergence is reproducible and
quotable in a bug report, and one loud regression cannot evict every other
record from the ring.

Two labelled metrics carry the same signal into your dashboards:

- `autumn_shadow_comparisons_total{version, route, outcome}` — `outcome` is
  `match`, `diverged`, `error` (the candidate could not be reached), `timeout`,
  `skipped` (a body over the capture budget), `dropped` (the in-flight ceiling
  was full), `refused` (the live build answered `429`/`503`), or `incomplete`
  (the client never finished reading the primary response).
- `autumn_shadow_divergences_total{version, route, kind}` — the series to alert
  on; it stays at zero on a clean run.

Both are **built-in families**, rendered by `/actuator/prometheus` alongside
`autumn_http_*`; they carry the same `version` label the canary cohort metrics
do, so a shadow run and a canary can be read on one dashboard.

The `route` label is axum's **matched route template** (`"/api/orders/{id}"`),
falling back to the configured pattern and then to `"*"`. Never the raw URL: an
unbounded URL space must not become unbounded metric cardinality. Past 200
distinct routes, further ones fold into `__other__`.

### PII in recorded samples

Recorded samples pass through the same `[log] filter_parameters` /
`[log] unfilter_parameters` redaction the access log, error pages, and failure
capsules use, and encrypted column names are always filtered. The recorded
request target is redacted the same way, matching on the percent-decoded
parameter name and on each of its structural segments — so `?token=`,
`?%74oken=`, `?auth[access_token]=` and `?filter.password=` are all caught.

An excerpt is recorded only when **every scalar in the body has an object key
above it** — because the filter replaces a matched key's whole value, so that
is exactly when naming a key could reach it. `{"tags": ["x"]}` qualifies:
listing `tags` redacts the array entire. What does not qualify is a value with
no key anywhere above it — an HTML or binary body, a bare scalar (a
`text/plain` one-time code parses as a JSON number), or a top-level array of
strings. Those record the digest, the byte length, and how the body was
normalized — enough to prove the builds disagree, without an excerpt no
redaction rule could vet.

**Know what that redaction is and is not.** It is a *key-name allowlist*: a JSON
field is replaced only when its name matches `[log] filter_parameters` (whose
defaults are `password`, `token`, `secret`, `api_key`, `ssn`, `credit_card`, and
a handful more). Every other field of the response body is recorded verbatim —
`email`, `phone`, `address`, `balance`, `csrf_token`, a top-level JSON string.
This is a category of data no other Autumn surface writes down, so before
enabling `[shadow]` on a route that returns personal data:

- scope `routes` to endpoints whose bodies you are willing to see in
  `/actuator/shadow`,
- extend `[log] filter_parameters` with the field names those endpoints return,
- and keep `[actuator] sensitive = false` anywhere the endpoint could be reached
  by someone who should not read production responses.

The recorded request target and the samples are published **only** on the
sensitive actuator endpoint. The `WARN` log line for a divergence deliberately
carries just the route, the kind, and the fingerprint — never the target or a
sample — and is emitted once per distinct divergence rather than once per
occurrence.

### ⚠️ Before you turn this on

- **The candidate receives live credentials.** Cookies and `Authorization`
  headers are forwarded, because a candidate that cannot authenticate answers
  every protected route with a `401` and the diff degenerates into noise. Treat
  the shadow target as exactly as trusted as production.
- **The candidate's own side effects are real.** Autumn guarantees the mirror
  does not touch *primary* state. It cannot stop the candidate from writing to a
  database you pointed it at. Contain it by **environment** — a scratch
  database, a read-only replica, a build with writes disabled.
- **`X-Autumn-Shadow` is a convenience signal, not a security boundary.** It is
  an ordinary request header, so anything that can reach your app can set it.
  Using it inside the candidate to skip writes is fine *as a second line*; using
  it as the only thing standing between mirrored traffic and your production
  database is not, and on the primary it would be a client-controlled kill
  switch for whatever you gated on it. (A client that sets it on production
  traffic also opts that request out of being mirrored, which is the header's
  loop guard doing its job.)
- **Mirroring is extra load** on the candidate and on this process's outbound
  connections. Start at a low `sample_rate` and a narrow `routes` allowlist.
- **Expect benign divergences.** Timestamps, generated ids, and CSRF tokens in a
  response body differ between two builds by construction. This slice has no
  fuzzy-tolerance knob (deliberately); scope `routes` to the endpoints whose
  bodies are deterministic.

---

## Choosing a strategy

| Situation | Recommended strategy |
|---|---|
| Routine feature release, backward-compatible schema | Rolling |
| Destructive schema change (column drop, rename) | Rolling + expand/contract migration pattern |
| High-risk release requiring fast rollback | Blue/green |
| Incident response — stop traffic immediately | [Maintenance mode](maintenance-mode.md) |
| Gradual rollout with automated promotion | [Canary](#canary-deploys) — platform traffic weights gated on Autumn's version-labelled metrics, with `autumn canary rollback` for a clean drain |
| Prove a build is behaviour-equivalent before it serves anyone | [Shadow](#shadow-differential-deploys) — mirror real `GET`/`HEAD` traffic to a candidate and diff responses request-for-request |
| Several VPS hosts you own, no orchestrator | [`autumn deploy up` with `[deploy] hosts`](fleet-deploys.md) — serial rolling deploy driven by the CLI, per-host blue/green, one migration per fleet |
| A whole fleet must pause writes at once | [`autumn deploy maintenance on`](fleet-deploys.md#runbook-a-fleet-wide-maintenance-window) — note it gates traffic, it does not drain hosts from the load balancer |

---

## Next steps

- **Verify before you ship**: `autumn migrate check` classifies SQL by
  rolling-deploy risk. Wire it into CI before `autumn migrate` runs.
- **Harden startup**: `autumn doctor --strict` in CI catches misconfigured
  secrets and missing environment variables before the image is built.
- **Monitor drains**: the `autumn_shutdown_aborted_requests_total` metric
  increments when a request is abandoned during shutdown. Alert on it to catch
  an undersized `shutdown_timeout_secs`.
- **Prove equivalence, not just health**: a [shadow run](#shadow-differential-deploys)
  answers "does the candidate return the same bytes?" — the question cohort
  metrics structurally cannot.
- **Full cloud-native setup**: Kubernetes readiness probes, OTLP tracing, and
  structured logging are covered in the [Cloud-Native Guide](cloud-native.md).
