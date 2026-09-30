### Fixed

- **circuit_breaker:** a `CircuitBreakerPolicy` with an unrepresentable
  `open_duration` (e.g. `Duration::MAX`) no longer panics on
  `Instant + Duration` overflow when the breaker trips; the open deadline
  saturates to the same far-future horizon the idempotency store uses
  (PR #1642).
