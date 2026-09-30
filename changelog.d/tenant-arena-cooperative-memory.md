### Added

- **Typed cooperative tenant scratch arena:** `TenantArena::try_bytes` and
  `try_string` bind each supported scratch allocation to its RAII quota
  charge, propagate quota exhaustion as HTTP 503, and retain ownership safely
  through eviction until final reclamation. ADR 0012 and a Verus lifecycle
  specification precisely reject hard-isolation/RSS claims: ordinary Rust,
  framework, third-party, stack, allocator, and native allocations remain
  outside this cooperative tracked-memory boundary. A live arena allocation
  keeps its tenant's accounting domain alive across eviction, so a later
  request rebinds it instead of admitting a second full-quota generation. Arena
  allocation failures retain `TryReserveError` directly (without allocating an
  error `String` under memory pressure).
