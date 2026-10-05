### Breaking Changes

- **Breaking:** `CircuitBreakerPolicy` has three new public fields:
  `slow_call_duration_threshold`, `slow_call_rate_threshold` and
  `cancelled_call_outcome` (issue #3060). A struct literal must set them or
  use `..CircuitBreakerPolicy::default()`
  ([migration guide](docs/migrations/next.md)).

### Added

- **resilience:** the circuit breaker opens on slow calls (issue #3060). Set
  `slow_call_duration_threshold_ms` and `slow_call_rate_threshold` under
  `[resilience.circuit_breaker.defaults]` or a host override.
- **resilience:** a call cancelled at or after the slow-call threshold counts
  as a slow call. `cancelled_call_outcome = "failure"` counts it as a failure.
  A call cancelled before the threshold counts as nothing.
- **resilience:** `/actuator/prometheus` shows
  `autumn_circuit_breaker_slow_calls_total` and
  `autumn_circuit_breaker_slow_call_ratio` for each breaker.
  `/actuator/circuitbreakers` shows `slow_call_ratio`.

### Changed

- **resilience:** slow-call detection is on by default: a call of 60 s or more
  is slow, and the breaker opens when all calls in the window are slow. Set
  `slow_call_duration_threshold_ms = 0` to turn it off.
- **resilience:** the sample window is a ring of 10 counter buckets. Memory is
  constant at all request rates. A call stays in the window for 9/10 to 10/10
  of `sample_window_secs`.
