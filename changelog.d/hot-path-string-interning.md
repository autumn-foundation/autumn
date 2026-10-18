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
