### Performance

- **http:** the access log, the request-timeout layer and the log context
  keep the request's matched route and request id as the shared values the
  request already carries, instead of copying them into a `String` on every
  request. The trusted-host check no longer lowercases a fresh copy of an
  already-normal `Host` header. Log output is unchanged.
- **proxies:** behind a trusted proxy, reading `X-Forwarded-For` no longer
  copies the chain on every resolve. Resolving the client address
  allocates nothing for a one-line chain (it made 2 allocations per call).
- **db:** per-route DB query metrics no longer build their
  `"{method} {route} {verb}"` keys as `String`s on every request and query.
  Recording under a key already seen allocates nothing (it made 1–3
  allocations per sample). Metric keys are unchanged.
- **rate limiting:** a warm `#[throttle]` check makes 3 allocations instead
  of 13. The limiter registry and the per-client buckets are probed by
  borrowed key. On the Redis backend the Lua script is built once instead of
  hashed on every decision, and the three 429 bodies are rendered once.
- **i18n:** locale negotiation and the ambient locale scope share the
  configured locale list and fallback chain instead of copying them per
  request, and a `Translated` render no longer copies the active tag.
- **realtime:** channel metrics, presence and the collaborative-editing hub
  look up topics and members by borrow instead of copying the name on every
  publish, join or cursor move. A presence stream counts members without
  cloning the member list. The Redis channel backend encodes its envelope
  without copying the payload, and the collaborative text CRDT no longer
  copies each operation it applies. Wire formats are unchanged.
- **security:** CSRF reuses the request's cookie token instead of copying
  it several times per request, and CSRF and submit-token skip path
  normalization when no exempt paths are configured. A successful response
  no longer pays to render the error-page context's request id, method,
  query and cookies; only error responses get that context. The log
  parameter filter normalizes field names on the stack.
- **generated code:** repositories with `broadcasts` or version history no
  longer copy static topic names, OOB container ids or the actor on every
  write. `#[state_machine]` `on_commit` effects serialize from borrowed
  names to the same job payload.
- **jobs:** the job claim loop, queue-slot accounting and the actuator's job
  gauges borrow queue names instead of copying them per claim and per job.
- **tenancy:** tenant bulkhead permits and tenant cell handles share one
  tenant id. Feature-flag rollout and experiment bucketing hash their key in
  parts instead of building a joined string; bucket assignments are
  unchanged.
