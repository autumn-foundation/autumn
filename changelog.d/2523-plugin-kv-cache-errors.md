### Fixed

- **plugin-sandbox:** a cache-backed plugin KV store now reports backend
  failures to the guest (issue #2523). A failed write is no longer reported as
  success. An outage is no longer read as a missing key. `Cache` gains
  `try_get_value`, `try_insert_raw_bytes` and `try_invalidate`. Each has a
  default, so other backends need no change. A wrapper cache must forward them.
  `RedisCache` overrides them. New items: `try_get_cached`,
  `try_insert_cached` and `CacheBackendError`.
