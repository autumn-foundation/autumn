### Fixed

- **cli:** Fix the source-overwrite guard of `autumn db scrub --artifact`
  (issue #2366). The guard no longer refuses the profile that you scrub.
  The staging drill now works with an `autumn-staging.toml` file. The guard
  also compares shard URLs. It reads the host of a URL without a host from
  `hostaddr`, `host` or `PGHOST`.
