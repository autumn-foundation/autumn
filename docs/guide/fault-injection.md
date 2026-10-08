# Staging Fault Injection

Add latency and errors to a staging app. Use it to test timeouts, retries,
fallbacks and alerts before a real outage tests them. You do not need a
service mesh or Toxiproxy.

> **Warning:** do not use fault injection in production. The framework
> refuses it in the `prod` profile unless you set
> `allow_in_production = true`.

---

## Declare faults

Add a `[fault_injection]` section and one `[[fault_injection.faults]]` table
for each fault:

```toml
[fault_injection]
enabled = true

# 5 % of /api requests fail with 503 before the handler runs.
[[fault_injection.faults]]
routes = ["/api/*"]
target = "route"
kind = "error"
rate = 0.05
status = 503

# 10 % of database checkouts on /orders wait 300 ms.
[[fault_injection.faults]]
routes = ["/orders*"]
target = "database"
kind = "latency"
rate = 0.1
latency_ms = 300
```

| Key | Default | Meaning |
| --- | --- | --- |
| `routes` | all paths | Path patterns. A trailing `*` matches a prefix. Other patterns match the exact path. |
| `target` | `"route"` | Where the fault occurs. See [Targets](#targets). |
| `kind` | required | `"latency"` or `"error"`. |
| `rate` | required | The probability, from `0.0` to `1.0`. |
| `latency_ms` | `0` | The added wait for `kind = "latency"`. From 1 to 300000. |
| `status` | `503` | The status for a route error. From 400 to 599. |

Set `AUTUMN_FAULT_INJECTION__ENABLED=true` to turn on the faults for one
deployment. The faults are still in `autumn.toml`.

## Targets

| `target` | The fault occurs in |
| --- | --- |
| `route` | The request, before the handler runs. The response has the header `x-autumn-fault: injected`. |
| `database` | Each `Db::checkout` (the `Db` extractor and the shard paths). An error is a `503`. |
| `redis` | Each Redis session store operation (load, save, destroy). |
| `http` | Each call through `http_client::Client`. An error is `ClientError::FaultInjected`. |

A dependency fault applies only in a request that matches `routes`. Work that
runs outside a request (jobs, the scheduler, spawned tasks) gets no faults.

The rolls use the app entropy source. A test with `SeededEntropy` gets the
same faults on each run.

## Safety rules

- **Refused in prod.** In the `prod` profile, config validation fails, and
  the app does not start. To allow it, set `allow_in_production = true`. An
  environment variable cannot set that key.
- **Probes are exempt.** The health, liveness, readiness and startup paths,
  and the actuator paths, never get a fault.
- **Stop condition.** See [Stop condition](#stop-condition).
- **Audit.** Each toggle writes an audit event and a `warn` log on the
  `autumn.fault_injection` target. See [Audit](#audit).

## Stop condition

The framework counts the requests and the `5xx` responses in a window. The
count includes injected errors, real errors, and timeouts. A request that
stops before it completes counts as an error when a fault fired in it.

When the error ratio burns the error budget too fast, the faults stop:

```toml
[fault_injection.stop]
objective = 99.0      # percent, as in [[slo]]
max_burn_rate = 14.4  # the fast-page burn rate
window_secs = 60
min_requests = 20
```

With these defaults, the faults stop when more than 14.4 % of the requests in
a 60-second window fail, and the window has 20 or more requests. The burn
rule is the same as the one in [SLOs as Code](slo.md).

The faults stay off until an operator starts them again.

## The handle

Get the handle from the app state:

```rust
use autumn_web::fault_injection::FaultInjection;

if let Some(faults) = state.extension::<FaultInjection>() {
    faults.disarm("ops@example.com", "game day over").await;
    faults.arm("ops@example.com").await;
    let snapshot = faults.snapshot();
    tracing::info!(armed = snapshot.armed, injected = snapshot.injected);
}
```

The handle is absent when the section is disabled or refused.

## Audit

Each toggle writes an `AuditEvent` to the installed audit sinks:

| Action | When |
| --- | --- |
| `fault_injection.armed` | The app starts with faults, or `arm` runs. |
| `fault_injection.disarmed` | The stop condition trips, or `disarm` runs. |

The event has the actor, and the `reason` and `profile` metadata. Install a
sink with `AppBuilder::with_audit_sink`. See [Audit Logging](audit-logging.md).

## Tests

`FaultPlan` (see [Simulation Testing](simulation-testing.md)) is the tool for
deterministic tests. This section is for a running staging app.
