### Fixed

- **plugin-sandbox:** a cache-backed plugin KV store now reports a backend
  failure to the guest (issue #2523). A lost write is no longer `Done`, and an
  outage is no longer a missing key. `Cache` gains default-implemented
  `try_get_value`, `try_insert_raw_bytes` and `try_invalidate`. `RedisCache`
  overrides them. Other backends need no change.
