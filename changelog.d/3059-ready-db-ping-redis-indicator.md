### Breaking Changes

- **Breaking:** `HealthConfig` gains the public fields `cache_ttl_ms`,
  `ping_timeout_ms`, `db_readiness` and `redis_readiness`
  ([migration guide](docs/migrations/next.md)). A struct literal over it
  needs `..HealthConfig::default()`.

### Fixed

- **probes:** `/ready` no longer reads primary pool saturation (issue #3059).
  A busy pool made a healthy replica unready. An idle pool made a dead
  database look ready. Now `/ready` sends `SELECT 1` to the primary on one
  dedicated connection outside the pool. A failed ping, or one slower than
  `health.ping_timeout_ms`, makes `/ready` return `503`.
- **probes:** the read-replica check and the `db:shard:<name>` indicators use
  the same dedicated-connection ping. A busy replica or shard pool does not
  make them fail.
- **actuator:** the `db` component of `/actuator/health` uses the same cached
  ping. It no longer goes `DOWN` when the pool is busy. With
  `health.detailed = true`, its details show the ping `error`.

### Added

- **probes:** `health.cache_ttl_ms` (default `1000`) keeps each database ping
  result and each health indicator result for a short time. When a result is
  stale, one refresh runs and the other probes wait for it. Concurrent probes
  cause a maximum of one ping in each TTL period. A prober that disconnects
  does not stop the refresh. `0` turns the cache off.
  `HealthIndicatorRegistry::set_cache_ttl` sets it for a registry you build.
- **probes:** `health.ping_timeout_ms` (default `2000`) is the time limit for
  one built-in database or Redis ping. `0` is refused at startup.
- **probes:** `health.db_readiness = false` keeps a replica in rotation when
  the primary ping fails. `/actuator/health` still shows `db` as `DOWN`.
- **probes:** a change of the database ping result writes a `warn` (failed)
  or `info` (recovered) log event.
- **redis:** `RedisHealthIndicator` sends `PING` with a time limit. The
  framework registers `redis:<subsystem>` for each subsystem whose config
  selects Redis (cache, channels, idempotency, jobs, rate limit, sessions,
  submit tokens, webhook replay). Subsystems on one URL share one connection.
  The indicators show in `/actuator/health` only. Set
  `health.redis_readiness = true` to make them gate `/ready`.
- Env: `AUTUMN_HEALTH__CACHE_TTL_MS`, `AUTUMN_HEALTH__PING_TIMEOUT_MS`,
  `AUTUMN_HEALTH__DB_READINESS`, `AUTUMN_HEALTH__REDIS_READINESS`.

### Changed

- **probes:** each replica with a database keeps one more connection open
  for the readiness ping (one more for a read replica). Count it in your
  Postgres `max_connections` budget.
- **test:** `TestApp` applies `health.cache_ttl_ms`. A test that changes a
  health indicator and reads it again in less than 1 s gets the cached
  result. Set `health.cache_ttl_ms = 0` in the test config.
- **probes:** saturation is not a readiness signal. To shed load, set
  `server.max_concurrent_requests`. It is off by default. When on, excess
  requests get `503` with `Retry-After`.
