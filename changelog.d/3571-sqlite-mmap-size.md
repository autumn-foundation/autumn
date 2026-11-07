### Added

- **sqlite:** opt-in memory-mapped reads and page cache size (issue #3571).
  `database.mmap_size` (bytes) sets `PRAGMA mmap_size`.
  `database.cache_size_kib` sets `PRAGMA cache_size`. Both apply to each
  pooled SQLite connection, read-only connections too. `database.fleet.mmap_size` and
  `database.fleet.cache_size_kib` tune the fleet databases on their own. All
  four are `0` (off) by default. Env: `AUTUMN_DATABASE__MMAP_SIZE`,
  `AUTUMN_DATABASE__CACHE_SIZE_KIB`, `AUTUMN_DATABASE__FLEET__MMAP_SIZE`,
  `AUTUMN_DATABASE__FLEET__CACHE_SIZE_KIB`. Validation refuses values that
  SQLite cannot read. On a 32-bit target, it also refuses a total map over
  1 GiB. See
  [SQLite in production](docs/guide/sqlite-in-production.md#memory-mapped-reads-and-the-page-cache)
  for when mmap helps and its `SIGBUS` risk.

### Breaking Changes

- **Breaking:** `DatabaseConfig` and `DatabaseFleetConfig` have the public
  fields `mmap_size` and `cache_size_kib`. A struct literal must set them
  ([migration guide](docs/migrations/next.md)).
