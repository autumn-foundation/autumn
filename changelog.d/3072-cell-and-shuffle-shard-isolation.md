### Added

- **tenancy:** per-tenant bulkheads (issue #3072).
  `tenancy.max_concurrent_requests` caps the in-flight requests of one tenant,
  and `tenancy.max_db_connections` caps its database connections. A request
  over a cap gets `503`. Other tenants are not affected.
- **jobs:** tenant isolation for the `local` backend (issue #3072).
  `[jobs.tenants]` serves each queue round-robin by tenant, caps the running
  jobs of one tenant (`max_concurrent`), and gives each tenant a stable set of
  worker lanes (`lanes`, `lanes_per_tenant`) by shuffle sharding.
- **jobs:** shard-local job tables (issue #3072).
  `jobs.postgres.shard_local = true` makes `autumn_jobs` on each shard and
  runs workers for it. `enqueue_in_tx` on a shard connection then commits or
  rolls back with the shard's data, and a control-database outage does not
  stop the shard workers. See
  [ADR 0018](docs/adr/0018-shard-local-framework-state.md).
- **cells:** `cell_router::CellRouter` maps a tenant to a cell with the shard
  slot hash. The new [cell isolation guide](docs/guide/cell-isolation.md)
  gives the cell deployment pattern and the multi-region stance.

### Breaking Changes

- **Breaking:** `TenancyConfig`, `JobConfig` and `JobPostgresConfig` have new
  public fields. A struct literal needs `..Default::default()`
  ([migration guide](docs/migrations/next.md)).
