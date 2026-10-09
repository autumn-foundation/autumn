# ADR 0016: Adaptive Admission Control and Client-Side Throttling

- Status: Accepted
- Date: 2026-10-07
- Deciders: Autumn maintainers
- Tags: resilience, overload, admission-control, load-shedding
- Supersedes: "Alternatives Considered" item 2, and the per-route part of
  item 3, of [ADR 0009](0009-adopt-overload-protection-load-shedding.md).
  Per-tenant and per-principal ceilings stay deferred.

## Context

ADR 0009 added `LoadShedLayer`: a static ceiling on in-flight requests. It
deferred three items: adaptive limits, priority shedding, and per-route
ceilings. Issue #3068 asks for them.

A static ceiling has two problems:

- It is correct for one latency only. When a dependency slows down, the same
  ceiling admits too much work, and the queue grows.
- It treats all requests the same. A batch export and a checkout payment are
  shed at the same point.

The client side has a matching problem. When a host rejects most calls, the
client sends them anyway, and retries add more load.

## Decision

### 1. Adaptive limit

Add `server.admission.mode = "adaptive"`. The ceiling then follows measured
latency. Admission stays lock-free and does not allocate: it reads the limit
with one atomic load and takes a slot with a CAS. Probes stay exempt.

Three algorithms, ported from Netflix `concurrency-limits`:

| Algorithm | Signal | Note |
| --- | --- | --- |
| `gradient2` (default) | latest RTT vs. long-term average | Samples are averaged over windows of ≥ 1 s and ≥ 10 samples. |
| `vegas` | estimated queue from the lowest RTT | Probes a new base RTT only when the limit is stable. |
| `aimd` | `504`, or RTT above `latency_threshold_ms` | Simple. Slow to grow. |

Each admitted request gives one sample when its response head is ready. A
`504` or an inner error is a drop. These give no sample, because their latency
does not show capacity:

- a `4xx` or `503` response (rate limit, not found, maintenance mode);
- a route with `timeout = "off"` (for example, a long poll);
- a request that the client cancels.

The request-timeout layer puts the request deadline on the request. If the
timeout cancels the request at that deadline, the layer records a drop with
the elapsed time. Thus a dependency that hangs lowers the limit.

The algorithm state is behind a mutex. A sample uses `try_lock`. When the lock
is busy, the limiter ignores the sample. A request does not wait for a lock.

`max_limit` defaults to the static ceiling (`max_concurrent_requests`, the
capacity contract, or the profile default). Thus the static ceiling stays a
hard cap.

### 2. Criticality partitions

A route can set `criticality = "critical" | "default" | "sheddable"`. Each
class can fill a share of the limit:

```text
sheddable_share <= default_share <= critical_share = 1.0
```

A request of class `c` is admitted only while `in_flight < floor(limit ×
share(c))`. The shares are nested. Thus, at any in-flight count, if the server
rejects a higher class, it also rejects every lower class.
`verification/admission_partitions.rs` proves this.

Defaults: `default = 1.0`, `sheddable = 0.5`. Routes without a criticality
keep the full limit, so the defaults change nothing for existing apps. To keep
headroom for `critical`, set `default` below 1.0.

The class is sent to downstream services in `X-Autumn-Criticality`, as Google
SRE describes. `default` is not sent, because a missing header means
`default`. The server reads that header only when
`trust_criticality_header = true`, because a public client could otherwise
mark itself `critical`.

### 3. Client-side adaptive throttling

Add `[http.client.adaptive_throttle]` (off by default). It is the Google SRE
algorithm: per host, over a sliding window, reject a new attempt locally with
probability

```text
max(0, (requests − K × accepts) / (requests + 1))
```

`429`, `503` and transport errors are not accepts. A call that the caller
cancels counts as an accept. On the plain path, each attempt is counted,
retries too. The custom path (`pin_to`, `get_ssrf_safe`, redirect modes) uses
the throttle only with `breaker_scoped()`, once per call: its hosts often come
from users. A throttle tracks at most 4096 hosts. A throttled attempt ends the
call with `ClientError::ThrottledLocally`, which maps to `503`, and does not
count as a circuit-breaker failure. The random draw uses the app's entropy, so
a sim seed replays it.

### 4. MCP `tools/call` admission

The envelope holds a slot as `critical`, because the tool is not known yet.
The replay must use its own class. Two rules (issue #3186):

- **Class slots.** The layer keeps a count of classified requests: direct
  requests and replays. An envelope is not in it. A replay claims a slot with a
  CAS on this count against the threshold of its class. Thus N+1 concurrent
  `sheddable` calls shed exactly one, and other unclassified envelopes do not
  shed a replay. A replay keeps the slot of its envelope and does not take a
  second one in the total count.
- **No sample.** The layer gives the envelope an `EnvelopeAdmission` handle in
  the request extensions. The replay gets the handle. When the replay is shed,
  it marks the handle, and the envelope gives no limiter sample. MCP answers a
  shed replay with HTTP 200, so the sample would be fast and false.

Alternative rejected: read the body before admission and admit the envelope at
the tool's class. It needs the body in memory before the first shed decision.

### 5. Deadline-expired drops

The issue asks to drop queued requests whose deadline passed. This limiter does
not queue: it admits or sheds at once. Thus no request waits past a deadline in
the limiter. Deadline headers are the scope of issue #3058.

## Consequences

### Positive

- The ceiling follows the system. A slow dependency lowers the limit, and
  admitted requests keep a bounded latency.
- Operators choose what to shed first.
- Clients stop sending to a host that rejects most calls.
- No change for apps that set nothing.

### Negative

- `Route`, `ServerConfig` and `HttpClientConfig` have new public fields.
  Struct literals must add them. See `docs/migrations/next.md`.
- An adaptive limit is harder to reason about than a fixed number. The
  `autumn_admission_limit` gauge shows its value.
- After a long latency increase, the Gradient2 limit can stay low for
  minutes. Use `vegas` when that is a problem.
- The `/mcp` envelope admits a call as `critical` (up to the full limit),
  before it knows the tool. The `tools/call` replay then claims a slot at the
  tool route's class (see "MCP `tools/call` admission"). MCP methods other
  than `tools/call` (`initialize`, `tools/list`) are admitted up to the full
  limit.

## Evidence

- `sim_adaptive_admission`: with 5× upstream latency, the limit goes down
  within 20 s and admitted p99 stays ≤ 150 ms (3× the slow service time). A
  static ceiling at the adaptive start value does not meet that bound.
- `sim_admission_criticality`: `sheddable` is rejected first.
- `sim_client_throttle` and `admission::tests::throttle_*`: the client rejects
  locally only below the `1/K` accept ratio.
- `benches/admission.rs` (Criterion) and `tests/admission_alloc_gate.rs`:
  adaptive admission adds no allocation, and its time is within ε of static.

## Alternatives Considered

1. **Netflix reserved partitions** (each class owns a share; shares sum to 1).
   Rejected: a class can then exceed the total limit, and the order between
   classes is not strict.
2. **A priority queue.** Rejected for the reason in ADR 0009: a queue adds
   latency and does not bound memory.
3. **Per-sample Gradient2.** Rejected: the long-term average follows a high RTT
   within seconds, and the limit grows without bound under queueing. A unit
   test keeps this from coming back.
