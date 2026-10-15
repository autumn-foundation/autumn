### Fixed

- **cli:** Fix the source-overwrite guard of `autumn db scrub --artifact`
  (issue #2366). The guard no longer refuses because of the config overlay of
  the profile that you scrub. The staging drill now works with an
  `autumn-staging.toml` file. The guard still checks the profile that the
  artifact came from. It also compares shard URLs and keyword-form
  connection strings. It reads the host of a URL without a host from
  `hostaddr`, `host` or `PGHOST`.
