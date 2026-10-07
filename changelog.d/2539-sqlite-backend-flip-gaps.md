### Security

- **db:** the database-target redactor masks two more shapes (issue #2539,
  #2571). An opaque URL such as `postgres:password=hunter2` keeps only its
  scheme, unless the rest is one path-shaped token. An allowlisted query
  value, such as `application_name`, is masked unless it is a simple token,
  so a nested credentialed URL does not go into the boot log.

### Fixed

- **feature flags, experiments, runtime config:** `PgFlagStore::new(url)`,
  `PgExperimentStore::new(url)` and `PgConfigStore::new(url)` refuse a
  non-Postgres target at the first connect, with a message that names the
  cause. Before, libpq refused it and quoted the target with its credentials
  (issue #2539).
- **search:** `autumn-search` compiles under `autumn-web/sqlite`. On a SQLite
  build, boot refuses `SearchPlugin::postgres()` and the Postgres store
  refuses every query, with a clear message (issue #2539).
