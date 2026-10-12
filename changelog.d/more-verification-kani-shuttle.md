### Fixed

- **circuit_breaker:** a `CircuitBreakerPolicy` built in code with
  `half_open_trial_count: 0` no longer strands the breaker. A half-open breaker
  with 0 trials refused every call, so nothing could close it again.
  `CircuitBreaker::new` and `update_config` now clamp the count to at least 1,
  as the `[resilience]` config already did. The new Verus spec
  `verification/circuit_breaker.rs` found it.

### Testing

- **verification:** Kani proofs over the shipped backoff, admission
  partitions, AIMD limit, byte-range parser, outbox retry delay, certificate
  backoff and time helpers (`cargo kani -p autumn-web --lib`, `kani.yml`).
  Four new Verus specs: circuit-breaker half-open accounting, the retry
  budget token bucket, the tenant bulkhead and backoff jitter. Shuttle checks
  (`tests/shuttle_models.rs`) and new loom models drive the real
  `RetryBudget`, `TenantBulkhead`, `CircuitBreaker` and `AdaptiveLimiter`
  through a sync shim, instead of a copy of their algorithms.
