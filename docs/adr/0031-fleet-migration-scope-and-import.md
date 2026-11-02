# ADR 0031: Scoped Migration Sets and Fleet Import

- Status: Accepted
- Date: 2026-11-01
- Deciders: Autumn maintainers
- Tags: db, migrations, sqlite, fleet

## Context

Apiary, a test bed for the SQLite fleet (ADR 0019), found two gaps:

1. Every app migration set goes to the control database and to every fleet
   file. Control-plane tables then exist, empty, in each tenant file, and
   tenant tables exist, empty, in the control database.
2. There is no way to restore a local backup. Apiary used
   `FleetLifecycle::restore_missing` for it: stage the backup, open the key,
   and let the hook copy the file. The hook was not made for this, and it
   does not migrate or check the copy before it is published.

## Options

For scope:

1. `AppBuilder::control_migrations` and `AppBuilder::shard_migrations`.
2. `AppBuilder::migrations_for(MigrationScope, set)`.
3. Encode the scope in the set label.

For import:

1. Document the lifecycle hook as the supported way.
2. `DatabaseFleet::import(key, src)`.

## Decision

Scope: option 2. One method, and an enum that can grow. Each registered set
carries a `MigrationScope`. The control loops take the sets that reach the
control database. The shard loops and the fleet take the sets that reach the
shards. Each side computes its own version-collision map from its own sets.
A scoped set does not count when boot decides whether to add a framework
set: it misses one side.

Import: option 2. The import uses the provision turn of the key. It makes a
private staging file: a `VACUUM INTO` copy of the source (a plain copy
misses a `-wal`). It checks `PRAGMA integrity_check` and the identity row,
which must exist and record the key. It applies the migrations, and sets the
epoch. Then it purges a tombstone, and publishes the file with a hard link
under the path lock. The purge comes last, so a refused source keeps the
retained replica.

## Consequences

- `autumn migrate` and `autumn fleet migrate` read `migrations/` and do not
  know a scope. The guide tells the app to keep a scoped set in its own
  directory and to use `AUTUMN_MIGRATE=1`.
- The registered list is `(label, set, scope)`. The label is still unused.
- An import never replaces a file. To replace a database, delete it first.
- A file with no identity row cannot be imported.
- `autumn fleet import` stops when ownership is on: it holds no slot lease.
- `FleetLifecycle::restore_missing` keeps its behaviour. Its docs now say
  that each request-path open of a missing file asks it.
