# ADR 0039: Migration Scope for Plugins, the CLI, and One Database

- Status: Accepted
- Date: 2026-11-05
- Deciders: Autumn maintainers
- Tags: db, migrations, sqlite, fleet, cli
- Extends: ADR 0031

## Context

ADR 0031 added `MigrationScope` and `AppBuilder::migrations_for`. Issue
#3536 found four gaps:

1. A plugin cannot give its set a scope.
2. On an app with no shard and no fleet, a `Shards` set goes to no
   database.
3. A content test, not the scope, keeps the framework control-plane set off
   the shards. The test is in four places.
4. `autumn migrate` and `autumn fleet migrate` do not know a scope.

## Decision

1. Add `AppBuilder::plugin_migrations_for(name, scope, set)`. It has the
   same argument order as `migrations_for`. The enum keeps its names
   (`Everywhere`, `Control`, `Shards`). They also cover Postgres shards.
2. With no `[[database.shards]]` and no `[database.fleet]`, the control
   database takes every scope. Its collision map then reads every set.
3. `migrate::applied_scope(set, scope)` gives the scope that a set applies
   with. The framework control-plane set applies as `Control`. The shard
   loops and the fleet use it. `build_for_app` has no filter closure.
   Collision maps still read the registered scope. Thus a substitute version
   that a shard already records does not change.
4. The CLI reads the scope from the directory name:
   `MigrationScope::directory` gives `migrations`, `migrations-control` and
   `migrations-shards`. An app embeds each directory with its scope.

Options we did not use:

- A scope key in `autumn.toml`. The app embeds its sets at compile time, so
  a config key and the code can disagree.
- The CLI runs the app binary with `AUTUMN_MIGRATE=1`. The binary may not be
  built, and the one-shot also migrates the control database.

## Consequences

- `autumn fleet migrate` applies `migrations/` and `migrations-shards/`, and
  it agrees with boot when the app embeds them with those scopes.
- `autumn migrate` applies `migrations/` and `migrations-control/` to a
  `SQLite` control database. With one database, it also applies
  `migrations-shards/`.
- On a Postgres target with a scope directory, `autumn migrate` stops before
  it applies anything. Its checksum check reads one directory. Use
  `AUTUMN_MIGRATE=1`.
- `autumn migrate down` and `status` read `migrations/` only.
- Plugin sets are not on disk. Only boot and `AUTUMN_MIGRATE=1` apply them.
