### Changed

- **cache:** the fill fence now works across replicas on `RedisCache`
  (issue #2356). `invalidate_namespace` raises a per-namespace epoch in Redis
  before it sweeps. A fill stores its value only if the epoch did not move, so
  a fill on another replica cannot write back an invalidated value. A `true`
  is now valid for all replicas. It is `false` if the epoch bump fails. The
  invalidation now needs a Redis write (`INCR`). Cost: one `GET` on each miss. Custom backends opt in with `Cache::shares_fill_epoch`,
  `Cache::fill_epoch` and `Cache::insert_raw_bytes_if_epoch`. Without them, the fence stays per
  process.
