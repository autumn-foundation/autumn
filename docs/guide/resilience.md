# Resilience & Outbound Circuit Breakers

Autumn provides a first-class `CircuitBreaker` resilience policy for outbound dependencies (HTTP clients, background jobs, and SMTP mailers) to protect the application from cascading failures during downstream outages.

---

## Core Concepts

A circuit breaker wraps outbound calls and tracks their success/failure ratio. It operates as a state machine with three states:

```mermaid
stateDiagram-v2
    [*] --> CLOSED
    CLOSED --> OPEN : Failure Ratio > Threshold\n(after Min Samples)
    OPEN --> HALF_OPEN : Open Duration Expires
    HALF_OPEN --> CLOSED : Trials Succeed
    HALF_OPEN --> OPEN : Trial Fails
```

- **CLOSED**: Requests pass through normally. Successes and failures are tracked in a sliding window.
- **OPEN**: Requests fail fast immediately with a `503 Service Unavailable` error (`ClientError::CircuitBreakerOpen` for HTTP, `MailError::RuntimeUnavailable` for mailers, etc.) without contacting the remote dependency.
- **HALF_OPEN**: A limited number of trial requests are sent. If all trials succeed, the circuit closes again; if any trial fails, it immediately re-opens.

---

## Configuration

Circuit breakers are configured in `autumn.toml` under the `[resilience.circuit_breaker]` section. You can set global defaults and define per-host overrides.

```toml
# autumn.toml
[resilience.circuit_breaker.defaults]
failure_ratio_threshold = 0.5    # Trip when >= 50% of calls fail (default: 0.5)
sample_window_secs      = 10     # Track last 10 seconds of traffic (default: 10)
minimum_sample_count    = 10     # Require at least 10 calls to trip (default: 10)
open_duration_secs      = 60     # Keep circuit open for 60 seconds (default: 60)
half_open_trial_count   = 3      # Run 3 trial requests in Half-Open (default: 3)

# Per-host overrides for outbound HTTP clients
[resilience.circuit_breaker.hosts."api.stripe.com"]
failure_ratio_threshold = 0.3
minimum_sample_count    = 5
open_duration_secs      = 30

[resilience.circuit_breaker.hosts."api.sendgrid.com"]
open_duration_secs      = 10
```

---

## Integrations

### Outbound HTTP Client
The outbound [Client](file:///c:/Users/markm/autumn/autumn/src/http_client.rs) automatically attaches a circuit breaker keyed by target host to every outgoing request.
- **Successes**: Any HTTP response with status `< 500`.
- **Failures**: Any network timeout, connection error, or HTTP status `>= 500`.

### Background Jobs
All background job enqueues (to Redis or PostgreSQL durable queues) in [JobClient](file:///c:/Users/markm/autumn/autumn/src/job.rs) are wrapped in a circuit breaker named `"job_queue"`. If the queue store experiences an outage, subsequent enqueue calls fail fast, preventing thread starvation.

### SMTP Mailer
Outgoing SMTP transport sends in [SmtpTransport](file:///c:/Users/markm/autumn/autumn/src/mail.rs) are wrapped in a circuit breaker named `"smtp_mailer"`. If the mail server goes down, mail sends fail fast immediately.

---

## Actuator Visibility

### Breaker State Endpoint
The `GET <actuator-prefix>/circuitbreakers` endpoint returns the current state of all active breakers.

- **Detailed Mode** (`health.detailed = true`):
  ```json
  [
    {
      "name": "api.stripe.com",
      "state": "CLOSED",
      "failure_ratio": 0.1,
      "failure_ratio_threshold": 0.3,
      "sample_window": "10s",
      "minimum_sample_count": 5,
      "open_duration": "30s",
      "half_open_trial_count": 3
    }
  ]
  ```
- **Undetailed Mode** (`health.detailed = false` in production):
  ```json
  [
    {
      "name": "api.stripe.com",
      "state": "CLOSED",
      "failure_ratio": 0.1
    }
  ]
  ```

### Health Integration & Downstream Outage Pattern
Every circuit breaker exposes its state as a `HealthIndicator` mapped under `components.circuit_breaker.<name>` on the `/actuator/health` endpoint.

To support the **Downstream Outage Pattern**, breaker health indicators are registered in the `HealthOnly` group:
- While a breaker is `OPEN`, `/actuator/health` returns `503 Service Unavailable` and displays status `DOWN` for that circuit.
- Crucially, the readiness probe endpoints `/health` and `/ready` remain `UP` (`200 OK`). This prevents Kubernetes from killing or removing the application replica from the load balancer pool simply because a third-party dependency (like Stripe or SendGrid) is down.

---

## Telemetry & Logging

Circuit state transitions are instrumented with the `tracing` ecosystem. Every transition emits a structured tracing event with attributes:
- `circuit.name`: The key of the circuit breaker (e.g. host name, `"job_queue"`, or `"smtp_mailer"`).
- `circuit.state`: The target state (`CLOSED`, `OPEN`, or `HALF_OPEN`).
- `circuit.failure_ratio`: The failure ratio that triggered the transition.

Example transition log:
```
INFO circuit_breaker: Transitioned to OPEN circuit.name="api.stripe.com" circuit.state="OPEN" circuit.failure_ratio=0.6
```

---

## Overload Protection & Load Shedding

Circuit breakers protect against a *downstream* dependency failing. Rate
limiting protects against a *greedy client*. Neither protects against the
*process itself* running out of capacity — a traffic spike, a slow query, or
a GC stall that causes admitted requests to pile up faster than they
complete. Left unbounded, that pile-up climbs RSS until the process is
OOM-killed: a full blackout that drops every in-flight request at once.

Autumn's answer is admission control: a single config knob caps concurrent
in-flight requests, and the excess is shed immediately with a `503 Service
Unavailable` + `Retry-After` — a brownout instead of a blackout.

```toml
# autumn.toml
[server]
max_concurrent_requests = 256   # unset by default (unlimited)
```

Override at runtime with `AUTUMN_SERVER__MAX_CONCURRENT_REQUESTS`. A
reasonable starting point is the number of worker threads times a small
multiple (2-4x); tune based on the observed `autumn_requests_shed_total`
counter (exposed at `/actuator/prometheus`) and per-route latency.

Key properties:

- **Disabled by default.** `None`/`0` preserves today's unlimited behavior —
  no existing application silently changes throughput.
- **Before the handler runs.** A shed request never reaches your handler or
  has its body read; the `503` is returned immediately.
- **Probes are never shed.** `/health`, `/live`, `/ready`, `/startup`, and
  the whole actuator prefix always pass through, so a merely-busy replica is
  never killed by its orchestrator.
- **Composes with graceful shutdown.** The admission counter is independent
  of the shutdown-drain accounting, so shedding never double-counts,
  deadlocks, or extends the drain budget.
- **Observable.** Every shed request increments `autumn_requests_shed_total`
  and is access-logged with `status = 503` like any other response.

See [ADR 0009](../adr/0009-adopt-overload-protection-load-shedding.md) for
the full design rationale and how this differs from rate limiting and
per-request timeouts.
