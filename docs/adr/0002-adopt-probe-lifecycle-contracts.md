# ADR 0002: Adopt Explicit Probe Lifecycle Contracts

- Status: Accepted
- Date: 2026-04-09
- Accepted: 2026-05-18
- Deciders: Autumn maintainers
- Tags: health, probes, operations, cloud-native, rolling-deploy, shutdown

## Context

Autumn currently exposes `/health` and `/actuator/health`, but the effective
contract is still a single health endpoint. That was acceptable for early
framework development, but it is too vague for orchestrated environments.

Load balancers, Kubernetes, and other schedulers need distinct answers to
distinct questions:

- Is the process alive?
- Is it ready to receive traffic?
- Has startup actually completed?

One endpoint cannot answer all three cleanly without becoming a semantic mud
puddle.

Autumn also needs better shutdown semantics. During graceful shutdown, Autumn
should stop receiving new traffic before it stops being alive. That means
readiness should fail before liveness.

## Decision

Autumn will adopt explicit lifecycle probes:

- `/live`
- `/ready`
- `/startup`

Autumn will retain `/health` during the transition as a compatibility endpoint
for `v0.x`.

## Probe Semantics

### `/live`

`/live` answers: "Should the runtime be restarted?"

It should return `200 OK` when:

- the process is running
- the router is mounted
- the runtime has not entered a terminally broken state

It should not fail merely because an external dependency is unavailable. A dead
database is a readiness problem first, not a liveness problem.

### `/ready`

`/ready` answers: "Is this replica safe to receive new traffic?"

It should return `200 OK` only when all required serving dependencies are ready.
That includes, when configured:

- database connectivity (a `SELECT 1` ping since #3059; pool availability is
  not a readiness signal)
- startup hook completion
- migration gate completion if the app is configured to wait for migrations
- required external state backends such as Redis-backed sessions, when
  `health.redis_readiness = true` (since #3059 the built-in Redis indicators
  are health-only by default)
- registered custom readiness checks

During graceful shutdown, `/ready` must flip to `503` before the server begins
draining.

### `/startup`

`/startup` answers: "Has bootstrapping completed yet?"

It should return `503` until startup hooks and required bootstrap gates have
completed. Once startup succeeds, it should return `200` and remain successful
unless the process is restarted.

## Extensibility

Autumn will provide an application-level readiness registration seam for custom
checks.

It will not provide arbitrary custom liveness hooks in Phase 1. Liveness should
remain narrow and hard to abuse.

## Configuration

Autumn will add explicit probe configuration with defaults:

- `live_path = "/live"`
- `ready_path = "/ready"`
- `startup_path = "/startup"`
- compatibility `health_path = "/health"`

The runtime must honor configured paths consistently across routing, startup
logs, and docs.

## Consequences

### Positive

- Operators get probe semantics that match how orchestrators actually work
- Graceful shutdown becomes easier to reason about
- Readiness can model real production dependencies
- Autumn moves away from ambiguous health semantics

### Negative

- More endpoints must be documented and maintained
- Probe state becomes a first-class part of `AppState`
- Users who only know `/health` need migration guidance

### Risks

- Allowing readiness to become too slow or dependency-heavy
- Confusing users if `/health` and `/ready` diverge without clear transition docs
- Overloading liveness with dependency checks and causing restart storms

## Alternatives Considered

### 1. Keep only `/health`

Rejected because it cannot express startup, readiness, and liveness cleanly.

### 2. Replace `/health` immediately

Rejected for now because Autumn still has existing users and examples relying on
`/health`. A compatibility window is cheaper than a rude surprise.

### 3. Make all probe semantics user-defined

Rejected because operators need consistent defaults across Autumn applications.
If every app reinvents probe meaning, the framework contributes nothing.

## Non-Goals

- Building a full health-check framework for arbitrary business diagnostics
- Turning liveness into a dependency monitor
- Adding per-endpoint authentication for probes in Phase 1

## Follow-On Work

- Implement probe lifecycle state in the runtime ✓ (done)
- Add readiness check registration to `AppBuilder` ✓ (done)
- Update CLI templates and docs to show probe usage ✓ (done)

---

## Addendum: Rolling Deploy Shutdown Contract (2026-05-18)

This addendum records the full rolling-deploy lifecycle decision that was
deferred at ADR acceptance time.

### Problem

The original ADR established probe semantics but did not define the complete
shutdown sequence or the ordering guarantees between phases. Downstream apps
had no falsifiable contract that SIGTERM → exit did not drop in-flight
requests, double-run jobs, or kill WebSocket sessions without a close frame.

### Decision

Autumn adopts the following ten-phase shutdown contract:

1. **signal_received** — SIGTERM or Ctrl-C arrives.
2. **ready_draining** — `/ready` returns `503` strictly before the listener
   closes.
3. **prestop_grace** — `server.prestop_grace_secs` (default `5`) elapses;
   configurable via `AUTUMN_SERVER__PRESTOP_GRACE_SECS`.
4. **ws_closing** — Open WebSocket sessions receive a `1001 Going Away` close
   frame.
5. **listener_stopping** — The TCP listener stops accepting new connections.
   `#[job]` workers and `#[scheduled]` tasks stop dequeuing; they share the
   same `CancellationToken` as the listener.
6. **in_flight_drain** — In-flight HTTP requests drain for up to
   `server.shutdown_timeout_secs`. Requests exceeding the deadline are aborted
   and counted in `autumn_shutdown_aborted_requests_total`; the process exits
   with code `1` and a structured log line naming the exceeded phase.
7. **app_hooks** — `on_shutdown` hooks run in LIFO registration order within
   the **remaining** portion of `shutdown_timeout_secs` after drain completes
   (drain and hooks share one budget, not two separate windows). Plugin hooks
   (registered during `build()`) run after app hooks (LIFO = last-registered
   first). Overruns are logged at WARN but do not block the remaining budget.
8. **telemetry_flush** — OTLP span exporter flushes (handled via guard drop).
9. **db_pool_close** — Connection pool drops with the process.
10. **exit 0** — Code `0` when all phases complete within deadlines;
    code `1` otherwise with a structured `phase=<name>` log event.

### Implementation

- `ServerConfig::prestop_grace_secs` (default `5`)
- `MetricsCollector::record_shutdown_aborted` populates
  `autumn_shutdown_aborted_requests_total`
- `run_shutdown_hooks_with_timeout` enforces per-hook and total budgets
- Integration tests in `autumn/tests/graceful_shutdown_contract.rs` assert
  the contract (AC 9: SIGTERM during long HTTP request; probe state during
  drain)

### Ordering rule (plugin vs app hooks)

**App hooks run before plugin hooks** during shutdown. Plugins register
during `build()` (before any `.on_shutdown()` in `main()`), so LIFO means
plugin hooks are earlier in the list and run last.

### Consequences

- No breaking changes to `on_shutdown` for existing users; `run()` calls the
  same hooks in the same order, now with per-hook timeouts.
- `prestop_grace_secs` adds a minimum `5`-second shutdown delay. Operators
  can set it to `0` to disable the delay (not recommended in production).
- `autumn_shutdown_aborted_requests_total` is now an observable SLI for
  deploy quality.

### Addendum (#3059)

Primary readiness is a cached `SELECT 1` on a dedicated connection, not pool
availability. A busy pool is load, not failure: pool saturation made busy
replicas leave rotation in turn. The built-in Redis indicators are
health-only by default, because all replicas share Redis. See
[Health Indicators](../guide/health-indicators.md#fail-open-on-shared-dependencies).
