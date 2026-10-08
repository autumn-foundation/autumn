# Staging Fault Injection

Add latency and errors to a staging app. Use it to test timeouts, retries,
fallbacks and alerts before a real outage occurs. You do not need a service
mesh or Toxiproxy.

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

# 5% of /api requests fail with 503 before the handler runs.
[[fault_injection.faults]]
routes = ["/api/*"]
target = "route"
kind = "error"
rate = 0.05
status = 503

# 10% of database checkouts on /orders wait 300 ms.
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
| `latency_ms` | required for `kind = "latency"` | The added wait, in milliseconds. From 1 to 300000. |
| `status` | `503` | The status for a route error. From 400 to 599. |

You can declare at most 64 faults. The total latency for one request or one
dependency call is at most 300 seconds.

To arm the faults for one deployment, set
`AUTUMN_FAULT_INJECTION__ENABLED=true`. You must declare the faults in
`autumn.toml`.

## Targets

| `target` | The fault occurs in |
| --- | --- |
| `route` | The request, after rate limiting and load shedding, before the handler. The response has the header `x-autumn-fault: injected`. |
| `database` | Each database connection checkout: the `Db` extractor, `LazyDb::checkout`, the shard paths and the generated repositories. An error is a `503`. |
| `redis` | Each Redis session store operation (load, save, destroy). |
| `http` | Each call through `http_client::Client`. An error is `ClientError::FaultInjected`. |

In an app with pre-rendered (SSG/ISG) pages, both fault layers go outside the
static cache, so a cached page can get a route fault too. There, a route fault
occurs before rate limiting and load shedding.

A dependency fault applies only in a request that matches `routes`. Work that
runs outside a request (jobs, the scheduler, spawned tasks) gets no faults.

The random decisions use the app entropy source. A test with `SeededEntropy`
gets the same faults on each run.

## Safety rules

- **Refused in prod.** In the `prod` profile, config validation fails, and
  the app does not start. The check ignores case, and an unset profile counts
  as `prod`. To allow it, set `allow_in_production = true` under
  `[profile.prod.fault_injection]`. Do not put it in the base section: then
  the environment variable alone arms the faults in prod.
- **Probes are exempt.** The health, liveness, readiness and startup paths,
  and the actuator paths, never get a fault.
- **Stop condition.** See [Stop condition](#stop-condition).
- **Audit.** Each arm and disarm writes an audit event and a `warn` log on the
  `autumn.fault_injection` target. See [Audit](#audit).

## Stop condition

The framework counts the requests that match a fault, and the bad results
among them, in a window. A result is bad when:

- the status is `5xx` (an injected error, a real error, or a timeout);
- an injected error fired, at any status;
- the request stops before it completes, and a fault fired in it.

The faults disarm when the burn rate is more than `max_burn_rate`:

```toml
[fault_injection.stop]
objective = 99.0      # percent, as in [[slo]]
max_burn_rate = 14.4  # the fast-page burn rate
window_secs = 60
min_requests = 20
```

With these defaults, the faults disarm when more than 14.4% of the matched
requests in a 60-second window are bad, and the window has 20 or more
requests. The burn rule is the same as the one in [SLOs as Code](slo.md).
Validation refuses values that let 100% of the requests fail.

Injected latency is bad only when it causes a timeout or a dropped request.
Set a request timeout in staging, so that slow requests count.

The faults stay disarmed until an operator calls `arm`.

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

Each arm and disarm writes an `AuditEvent` to the installed audit sinks:

| Action | When |
| --- | --- |
| `fault_injection.armed` | The app starts with faults, or `arm` runs. |
| `fault_injection.disarmed` | The stop condition trips, or `disarm` runs. |

The event has the actor and the `reason`, `profile` and
`allow_in_production` metadata. Install a sink with
`AppBuilder::with_audit_sink`. See [Audit Logging](audit-logging.md). With no
sink, the framework writes a `warn` at boot, and the events go to the log
only.

## Tests

For deterministic tests, use `FaultPlan` (see
[Simulation Testing](simulation-testing.md)). Use `[fault_injection]` only in
a running staging app.
