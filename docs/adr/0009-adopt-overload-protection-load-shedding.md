# ADR 0009: Adopt Overload Protection via Bounded-Concurrency Load Shedding

- Status: Accepted
- Date: 2026-07-06
- Deciders: Autumn maintainers
- Tags: resilience, overload, admission-control, load-shedding, cloud-native

## Context

Autumn already protects requests at the edges: per-request timeouts
(`server.timeouts.request_timeout_ms`), per-principal/IP rate limiting
(`security.rate_limit.*`), body-size limits, and a graceful-shutdown drain
that *counts* in-flight requests (see ADR 0002's shutdown-contract addendum).
Nothing, however, caps **total concurrent in-flight requests**. When work
arrives faster than it completes — a traffic spike, a slow upstream/DB, a
GC/allocator stall — admitted requests pile up unbounded. RSS climbs, latency
for *everyone* degrades, and the process marches toward an OOM kill: a full
blackout that drops every in-flight request at once. The framework can
already see the in-flight gauge (`MetricsCollector`'s `requests_active`,
consulted by the shutdown drain); it never enforced a ceiling on it.

Rate limiting answers "is this client being greedy?" — it cannot answer "is
this *process* out of capacity?", because a flood of distinct IPs (or one
legitimate viral event) sails past per-client buckets. Request timeout bounds
*duration*, not *count*: under overload it just means many slow requests die
at the deadline instead of failing fast. The missing primitive is **admission
control**: shed excess load with an instant `503 + Retry-After` so
already-admitted requests stay healthy and the load balancer gets a clean
"try another replica" signal — a brownout instead of a blackout.

### Prior art

- **axum / tower**: `GlobalConcurrencyLimitLayer` and `LoadShedLayer` exist,
  but are unwired and easy to get wrong — `ConcurrencyLimit` alone *queues*
  (backpressure) rather than sheds, so without explicitly pairing it with
  `LoadShed` a service still piles up admitted work. The user must hand-pick
  the ceiling, exempt probes, and wire metrics themselves.
- **actix-web**: offers connection-level `max_connections`, a coarse socket
  cap, not request-level admission with a clean `503 + Retry-After` and probe
  exemption.
- **Rails (Puma) / Django (gunicorn)**: no native request-level load shed;
  teams bolt on `rack-attack`/`rack-timeout` or front everything with nginx
  `limit_conn`. The capability lives outside the framework.
- **Phoenix**: leans on the BEAM scheduler and Cowboy/Bandit
  `:max_connections`; Plug has no built-in shed.
- **Go `net/http`**: nothing built in; the idiom is a hand-rolled
  semaphore-based middleware.

## Decision

Add a first-class, config-driven admission-control middleware
(`LoadShedLayer`) that caps concurrent in-flight requests and sheds the
excess with an immediate `503 Service Unavailable` + `Retry-After`, before
the handler runs or the request body is read.

### Configuration

A single additive `server.max_concurrent_requests: Option<usize>` field,
consistent with existing `server.*` keys (`shutdown_timeout_secs`,
`prestop_grace_secs`), settable via `autumn.toml` and
`AUTUMN_SERVER__MAX_CONCURRENT_REQUESTS`. `None` or `0` (the default)
disables the ceiling entirely — today's unlimited behavior — so no existing
application silently changes throughput. There is no separate on/off switch;
absence of a positive value *is* "off."

A reasonable starting point is the number of worker threads times a small
multiple (2-4x), sized to keep admitted-request tail latency stable under
the expected peak concurrency. Tune based on the observed
`autumn_requests_shed_total` counter and per-route latency.

### Admission mechanism

The ceiling is enforced by a dedicated `Arc<AtomicUsize>` counter, private to
the layer — **not** `MetricsCollector::requests_active` and **not** the
graceful-shutdown drain accounting. Admission is a lock-free
`compare_exchange` loop: O(1), no lock, negligible overhead below the
ceiling. A request is admitted only if doing so would not exceed the
configured limit; otherwise it is shed immediately. The counter is
decremented via an RAII guard held by the admitted request's future, so a
slot is released whether the request completes normally, errors, or is
cancelled/dropped mid-flight — mirroring the `PinnedDrop` guarantee
`MetricsFuture` already gives `requests_active`.

Because this counter is independent of the drain's `requests_active`,
shedding cannot double-count, deadlock, or extend the drain budget: entering
the drain phase (ADR 0002's `in_flight_drain`) does not interact with the
load-shed gauge at all, and a load-shed 503 short-circuits before either
counter changes.

### Probe exemption

Liveness/readiness/health probe routes (`health.path`, `health.live_path`,
`health.ready_path`, `health.startup_path`, and the actuator's own
`/health`, plus the whole actuator prefix) always bypass the gate,
uncounted. A merely-busy replica must not be killed by its orchestrator for
correctly shedding excess traffic. This reuses the exact bypass-path
construction `MaintenanceLayer` already uses (`server_config`'s health/probe
paths), factored into a single `probe_bypass_paths` helper so the two gates
can never drift apart.

### Response shape

The shed response is built via `AutumnError::service_unavailable_msg(...)`
— the same mechanism the built-in per-request timeout uses — so it flows
through the standard Problem Details / error-page stack: `application/
problem+json` for API clients, the framework's styled HTML error page for
browsers (negotiated by the outer `ErrorPageContext`/`ExceptionFilter`
layers, which preserve headers already on the response). A `Retry-After: 1`
header is added directly on the response, short enough that a client or load
balancer retries fast — or fails over to another replica — rather than
piling onto an already-loaded process.

### Placement in the middleware stack

The layer is applied adjacent to `MaintenanceLayer` — outer to it, so the
cheap in-flight-count check runs before maintenance mode's
bypass-header/IP-allowlist evaluation — and, transitively, inner to the
primary structured access log and the `Metrics` layer, and outer to CSRF,
rate limiting, body-size limits, and the handler. This guarantees the
`503` is issued **before** the handler runs or the request body is read
(the framework-wide contract for admission-style gates), and that the shed
outcome is still access-logged and countable.

### Observability

A dedicated counter, `autumn_requests_shed_total`, is incremented on every
shed request and exposed via `/actuator/prometheus`, following the exact
pattern of the existing `autumn_request_timeouts_total` and
`autumn_shutdown_aborted_requests_total` counters in `MetricsCollector`. The
structured per-request access-log line (see the access-log design) records
the outcome automatically: it logs whatever status code the response
carries, so a shed request appears as an ordinary `status = 503` line with
no schema change required.

## Success Metric

Under a synthetic overload (offered load = 2x the configured ceiling against
handlers that block ~200ms): p99 latency of *admitted* requests stays within
20% of the unloaded baseline and RSS stays bounded (no monotonic growth),
while excess requests receive a `503` in under 5ms rather than queueing to
the request timeout. Without the ceiling, the same load drives unbounded
in-flight count and RSS growth — the falsifiable contrast. This is measured
by `autumn dev-loop-bench --overload`, mirroring the existing cold-start and
scaling benchmark modes.

## Consequences

### Positive

- Bounded memory and stable admitted-request tail latency under overload,
  without any user-written tower layers.
- A clean, standard signal (`503 + Retry-After`) for load balancers to route
  around an overloaded replica instead of piling onto it.
- Reuses existing accounting/response/error machinery — no new response
  pipeline, no new probe-exemption logic to maintain in parallel.
- Zero overhead and zero behavior change for apps that never set the config
  key.

### Negative

- One more `server.*` config key to document and reason about when tuning
  a deployment.
- Choosing the right ceiling requires operator judgment (workload-dependent);
  the framework provides a starting-point heuristic but cannot auto-tune it
  in this slice.

### Risks

- A ceiling set too low sheds legitimate traffic during ordinary bursts;
  mitigated by making the default "off" and documenting a sizing heuristic
  tied to worker-thread count and observed shed-counter/latency telemetry.
- Confusing a shed `503` with a handler-level `503` (e.g. from the
  timeout middleware or a circuit breaker) without inspecting
  `autumn_requests_shed_total`; mitigated by the dedicated counter and by
  the access log always carrying the real wire status.

## Alternatives Considered

### 1. Wire `tower::limit::ConcurrencyLimitLayer` directly

Rejected as the sole mechanism: `ConcurrencyLimit` alone *queues* excess
requests (via `poll_ready` backpressure) rather than shedding them, so
without also pairing it with a shedding layer, admitted work still piles up
under sustained overload — exactly the failure mode this ADR exists to
prevent.

### 2. Adaptive / AIMD concurrency limits

Rejected for this slice (tracked as future work). A fixed, operator-tuned
ceiling is simpler to reason about and to falsify against the success
metric; auto-tuning (Netflix `concurrency-limits`-style) is a larger,
separate investment.

### 3. Per-route, per-tenant, or per-principal ceilings

Rejected for this slice. A single global limit is the simplest primitive
that satisfies the "protect the process from OOM" goal; finer-grained
ceilings are a natural, additive follow-up once the global mechanism is
proven.

### 4. Priority or fairness queueing

Rejected. This is pure load *shedding*, not a smarter queue — queueing adds
latency and complexity without changing the fundamental memory-bound
problem under sustained overload.

## Non-Goals

- Replacing or reworking rate limiting, per-request timeouts, or body-size
  limits — this is the complementary, missing third leg alongside them.
- Building a general-purpose backpressure/queueing framework.
- Auto-tuning the concurrency ceiling at runtime.

## Follow-On Work

- Implement `LoadShedLayer` + `server.max_concurrent_requests` ✓ (done)
- `autumn_requests_shed_total` counter + Prometheus exposition ✓ (done)
- `autumn dev-loop-bench --overload` benchmark harness ✓ (done)
- Consider adaptive/AIMD tuning and per-route ceilings as separate,
  evidence-driven follow-ups once this slice has production usage data.
