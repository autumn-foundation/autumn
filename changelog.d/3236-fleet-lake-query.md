### Added

- **db:** query the lake of a SQLite fleet (issue #3236, ADR 0032).
  `fleet.lake()?.materialize().await?.query(sql)` restores each live tenant
  from its `[replication]` replica into a scratch SQLite file and runs one
  `SELECT` across all tenants. Each table gets an `_autumn_tenant` column.
  The lake never opens the fleet root, so it runs on a host with no volume.
  Deleted tenants are skipped. See "Query the lake" in the SQLite fleet
  guide.
- **cli:** `autumn fleet lake <sql> [--out F] [--keep F] [--timestamp T]`
  runs one query over the replicas and writes CSV.
