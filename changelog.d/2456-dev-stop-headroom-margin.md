### Fixed

- **cli:** `autumn dev` now waits 15 s more than the managed-Postgres stop limit
  before it kills the app. A slow `pg_ctl stop` no longer leaves a running
  Postgres cluster behind (#2456).
