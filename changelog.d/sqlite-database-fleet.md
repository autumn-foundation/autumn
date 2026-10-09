### Added

- **sharding / sqlite:** a SQLite database fleet: one database file per
  tenant or per routing slot, behind the existing sharding extractors
  (ADR 0019). Configure `[database.fleet]` (`mode = "tenant"` or `"slot"`,
  `root`, an optional `path` template with `{tenant}`, `{slot}` and
  `{bucket}`). `ShardedDb`, `Shards`, `CrossShard` and
  `#[repository(sharded)]` then route each request to its tenant's own file,
  opened and migrated on first use, with no change to handlers. SQLite's one
  writer becomes one writer per tenant. Databases close when idle or past
  `max_open` (never with a connection checked out). `DatabaseFleet` provisions,
  deletes, backs up (`VACUUM INTO`), lists and restores tenant databases.
  Tenant ids that cannot safely name a file (`..`, `/`, uppercase, device
  names) are refused with `400`. `AUTUMN_MIGRATE=1` migrates the whole fleet.
  The `db:fleet` health indicator reports the counters.
- **replication / sqlite:** with `[replication]` on, every open fleet database
  ships its WAL to the same object storage under `fleet/tenant/<id>` or
  `fleet/slot/<n>`. Closing a database ships its last frames first.
  `database.fleet.restore_missing = true` rebuilds a database that is missing
  from the local volume out of its replica, so a host with a fresh volume can
  take over tenants or slots. `replication:fleet` reports the worst lag.
- **sharding:** `ShardSet::resolve` returns an owned `Shard` and serves both
  configured shards and a fleet; `Shard::metric_label` keeps metric labels
  bounded (`shard=fleet` for every fleet database).
