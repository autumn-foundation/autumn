### Added

- **db:** `database.fleet.min_residency` (default `"2s"`, env
  `AUTUMN_DATABASE__FLEET__MIN_RESIDENCY`). The `max_open` sweep does not close
  a database used within this time. Before, only `DatabaseFleet::builder` could
  set it. `DatabaseFleet::min_residency` reads it.
- **db:** `AppBuilder::migrations_for(MigrationScope, set)` keeps a migration
  set on the control database only (`MigrationScope::Control`) or on the
  shards and fleet databases only (`MigrationScope::Shards`). Boot and
  `AUTUMN_MIGRATE=1` obey the scope.
- **db:** `DatabaseFleet::import(key, src)` and `autumn fleet import <key>
  <src>` put a database file back, for example a backup. The import copies
  `src` with `VACUUM INTO`, checks its integrity and identity row, applies the
  migrations, and publishes the file in one step. It never replaces a file.
- **db:** `autumn_web::db::prelude`: Diesel's prelude with the `diesel_async`
  `RunQueryDsl` and `SaveChangesDsl`. Use it in place of `diesel::prelude::*`
  in a module with `#[model]` or `#[repository]`, where Diesel's prelude gives
  E0034.
- **repository:** a SQLite build also names the generated repository
  `Sqlite<Trait>` (a type alias of `Pg<Trait>`). `#[cached]` accepts both
  names.

### Changed

- **config:** the dev profile sets `session.secure = false`. Most clients do
  not send a `Secure` cookie back over plain http, so session sign-in failed
  under `cargo run`.
- **security:** a required `CsrfToken`, `CsrfFormField` or `CsrfTokenHeader`
  no longer answers `500` when CSRF is off. The token is empty, and the names
  are the defaults. `Option<CsrfToken>` is still `None` with CSRF off.

### Documentation

- **db:** the `max_open` docs say that the limit is soft under a burst, and
  give the file-descriptor budget for the peak open count. The SQLite fleet
  guide documents `FleetLifecycle::restore_missing` and the shared
  `libsqlite3-sys` with `autumn-harvest-sqlite`.

### Breaking Changes

- **Breaking:** `DatabaseFleetConfig` has the public field `min_residency`.
  A struct literal must set it
  ([migration guide](docs/migrations/next.md)).
