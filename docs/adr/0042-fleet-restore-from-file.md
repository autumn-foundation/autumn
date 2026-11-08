# ADR 0042: Fleet Restore From a Local File

- Status: Accepted
- Date: 2026-11-08
- Deciders: Autumn maintainers
- Tags: db, sqlite, fleet, replication

## Context

`DatabaseFleet::backup` writes a copy of one fleet database. Two paths read a
file back, and neither replaces a live database from a local file:

- `import` ([ADR 0031](0031-fleet-migration-scope-and-import.md)) never
  replaces a file. The key must be deleted first. With replication, the
  delete writes a tombstone.
- `restore` reads only the replica. Without replication it answers
  `NotReplicated`.

Apps used `FleetLifecycle::restore_missing` instead. That hook does not
check the identity row, does not stamp the epoch, and runs only for a
missing file.

## Options

For the API:

1. Let `import` replace a file.
2. A new call, `restore_from_file(key, src, LocalRestore)`.

For a deleted key with a tombstone:

1. Refuse it.
2. Clear the tombstone at once.
3. Refuse it, unless the caller asks to clear the tombstone.

For migrations:

1. Always apply them, as `import` does.
2. Follow the boot policy (`apply_migrations_on_open`), and let the caller
   ask for them.

## Decision

API: option 2. `import` keeps its rule that it never replaces a file. The
new call uses the import checks (`VACUUM INTO` copy, integrity check, plain
identity table, no trigger on the identity or epoch table, identity row,
migrations, epoch) on a staging file. The copy does not read the live file,
so it runs before the key goes out of service. Then the fleet swaps the
file with the swap step of the replica restore: path lock, exclusive guard,
old sidecars out, link in. The key is out of service for the whole swap, so
an opener waits and then sees the new file. The identity row is read again
after the migrations, because a trigger on another table can change it.

Tombstone: option 3. The tombstone keeps the replica of a deleted database
for undo. A silent clear would purge that replica. With
`LocalRestore::clear_tombstone`, the restore purges the old replica and
drops the row, as `provision` and `import` do. It takes the row under the
purge turn before the swap, because the replica task drops a row next to a
live file without the purge. It puts the row back when the swap fails, so
the undo replica stays. It purges after the swap. The new file has not
shipped yet, so the purge cannot reach it.

Migrations: option 2. Pending migrations refuse the restore when opens do
not apply them, and the live file stays. `LocalRestore::apply_migrations`
applies them. `autumn fleet restore --from` sets it, as `fleet import` and
`fleet migrate` apply migrations.

A backup of another key is refused (`FleetError::Restore`).
`LocalRestore::allow_rekey` accepts it and writes the target key into the
identity row.

With replication, the restore opens the database after the swap and ships
it. A new replicator starts a new generation, so the ship uploads a base
snapshot. A later replica restore then finds the restored data.

## Consequences

- The staging file sits beside the database, not under
  `RESTORE_CHECK_DIR`. A hard link needs one file system, and the
  database directory is on it.
- A ship that fails after the swap does not fail the call. The file is in
  place, and `LocalRestoreOutcome::ship_error` says why. The key must ship
  before any replica restore.
- The ship runs after the drain ends. A replica restore of the same key in
  that gap brings back the replica. Run restores of one key one at a time.
- `autumn fleet restore --from` stops when ownership is on: it holds no
  slot lease. The app calls `restore_from_file` instead.
