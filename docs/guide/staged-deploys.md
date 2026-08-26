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

## Choosing a strategy

| Situation | Recommended strategy |
|---|---|
| Routine feature release, backward-compatible schema | Rolling |
| Destructive schema change (column drop, rename) | Rolling + expand/contract migration pattern |
| High-risk release requiring fast rollback | Blue/green |
| Incident response — stop traffic immediately | [Maintenance mode](maintenance-mode.md) |
| Gradual rollout with automated promotion | [Canary](#canary-deploys) — platform traffic weights gated on Autumn's version-labelled metrics, with `autumn canary rollback` for a clean drain |
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
- **Full cloud-native setup**: Kubernetes readiness probes, OTLP tracing, and
  structured logging are covered in the [Cloud-Native Guide](cloud-native.md).
