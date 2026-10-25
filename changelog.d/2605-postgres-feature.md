### Breaking Changes

- **Breaking:** `db` no longer compiles the Postgres client. The new
  `postgres` feature (on by default) adds `pq-sys`, `tokio-postgres` and
  diesel's Postgres backend. A SQLite app with default features off now
  compiles none of them (issue #2605). A build that turns default features off
  and enables `db` without `sqlite` must add `postgres`
  ([migration guide](docs/migrations/next.md)).

### Fixed

- **http-client:** `--no-default-features --features http-client` compiles
  again. The capsule-replay check is behind `reporting`, which owns it.
