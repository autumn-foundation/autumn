### Added

- **Typed cooperative tenant scratch arena:** `TenantArena::try_bytes` and
  `try_string` bind each supported scratch allocation to its RAII quota
  charge, propagate quota exhaustion as HTTP 503, and retain ownership safely
  through eviction until final reclamation. ADR 0012 and a Verus lifecycle
  specification precisely reject hard-isolation/RSS claims: ordinary Rust,
  framework, third-party, stack, allocator, and native allocations remain
  outside this cooperative tracked-memory boundary. Evicted-but-live domains
  are weakly indexed and rebound for later requests, so LRU/TTL churn cannot
  reset usage and admit overlapping full-quota allocation generations. Arena
  allocation failures retain `TryReserveError` directly (without allocating an
  error `String` under memory pressure), and domain misses clean only their own
  stale weak entry rather than scanning the entire tenant index.
