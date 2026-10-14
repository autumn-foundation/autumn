### Fixed

- **cli:** `autumn db scrub --artifact` source-overwrite guard (issue #2366).
  It no longer refuses the profile you scrub, so the staging drill works with
  an `autumn-staging.toml`. It compares shard URLs. It resolves hostless
  PostgreSQL URLs through `host` and `PGHOST`.
