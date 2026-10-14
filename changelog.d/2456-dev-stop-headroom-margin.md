### Fixed

- **cli:** `autumn dev` waits 15 s longer than the managed-Postgres stop
  limit before it force-stops the app (issue #2456). A slow but correct
  `pg_ctl stop` no longer leaves an orphaned cluster.
