# SQLite Database Fleet

One SQLite database per tenant, or per routing slot, behind the same sharding
extractors a Postgres-sharded app uses. Design and research:
[ADR 0019](../adr/0019-sqlite-database-fleet.md).

SQLite has one writer per database. Put every tenant in one file and every
tenant's writes queue behind each other. Give each tenant its own file and the
writer is per tenant. A tenant's data is also one file: export it, restore it
or delete it with a file operation.

## When to use it

| Your app is… | Use |
| --- | --- |
| One host, write load fits one writer | [plain SQLite](sqlite-in-production.md) |
| One host (or one host per cell), many tenants, writes per tenant | **fleet, `mode = "tenant"`** |
| Very many small tenants; bounded file count | **fleet, `mode = "slot"`** |
| Many hosts sharing the same tenants live | [Postgres sharding](sharding.md) |

## Configuration

The fleet needs the `sqlite` feature and a file-backed `sqlite:` control
database. Sessions, jobs, flags and other framework state stay on the control
database. Tenant data goes to the fleet.

```toml
[database]
url = "sqlite:///var/lib/app/control.db"

[database.fleet]
mode = "tenant"                 # or "slot"
root = "/var/lib/app/fleet"     # on the host's local volume
# path = "{bucket}/{tenant}.db" # default for mode = "tenant"
max_open = 256                  # soft limit on open databases (LRU past this)
# max_resident_bytes = 0        # soft memory budget; 0 is off (LRU past this)
# min_residency = "2s"          # LRU does not close a database used this recently
pool_size = 2                   # connections per open database
idle_close_secs = 300           # close a database idle this long
# create_on_demand = false      # default: false for tenants, true for slots
# restore_missing = false       # see "Replication" below
# replica_retention_after_delete = "0s"  # see "Deleting a database's replica"
# restore_check_sample = 1      # see "Restore check"
# replication_workers = 4       # see "Replication lag"
# mmap_size = 0                 # bytes; see "Memory-mapped reads"
# cache_size_kib = 0            # KiB; 0 = SQLite default

# [database.fleet.ownership]    # see "Failover: slot ownership" below
# owner = "host-a"
# slots = ["0-8191"]
```

The control database must live outside `root`: otherwise a tenant or slot
path could resolve to it, so config validation (and boot, after resolving
symlinks) refuses that layout. An app with a custom `DatabasePoolProvider`
must name its control database with `DatabaseTopology::with_migration_url`,
or a fleet refuses to boot: it cannot otherwise tell where that pool points.

Every key has an `AUTUMN_DATABASE__FLEET__*` override. With no TOML section,
setting both `AUTUMN_DATABASE__FLEET__MODE` and `AUTUMN_DATABASE__FLEET__ROOT`
creates one.

### Path templates

`path` is relative to `root` and uses three placeholders:

| Placeholder | Renders | Example |
| --- | --- | --- |
| `{tenant}` | the tenant id | `acme` |
| `{slot}` | the routing slot, 5 digits | `00042` |
| `{bucket}` | `slot / 64`, 3 digits, 256 buckets | `000` |

A tenant fleet must use `{tenant}`; a slot fleet must use `{slot}`. The file
name ends in an extension such as `.db`, so SQLite's `-wal`/`-shm` sidecars are
never mistaken for databases. A bucket is 64 contiguous slots, so a host that
owns slots 0–8191 owns buckets `000`–`127`: moving a cell is moving
directories.

### Tenant ids

A tenant id becomes a file name, so a tenant fleet accepts only
`[a-z0-9_-]`, starting with a letter or digit, up to 128 bytes, and never a
Windows device name (`con`, `nul`, …). Anything else answers `400` and creates
nothing. Uppercase is refused rather than folded: on a case-insensitive file
system `Acme` and `acme` would share a file. If your ids are UUIDs, send them
lowercase.

A slot fleet takes any routing key: the key never names a file. So an app
that moves from tenant to slot mode loses this id check. Check ids where they
enter the app, for example with `TenantDbId::parse`
(`autumn_web::fleet_layout`).

## Handlers

Nothing changes. The fleet is the app's shard set:

```rust
#[post("/notes/{body}")]
async fn create(mut db: ShardedDb, Path(body): Path<String>) -> AutumnResult<String> {
    diesel::sql_query("INSERT INTO notes (body) VALUES (?)")
        .bind::<Text, _>(&body)
        .execute(&mut *db)
        .await?;
    Ok(db.shard().to_owned()) // "tenant:acme"
}

#[autumn_web::repository(Note, tenant_scoped, sharded)]
pub trait NoteRepository {}

#[get("/notes")]
async fn list(repo: SqliteNoteRepository) -> AutumnResult<Json<Vec<Note>>> {
    Ok(Json(repo.find_all().await?))
}
```

`#[repository]` generates `PgNoteRepository`. A SQLite build also names it
`SqliteNoteRepository`. The two names are one type. The generators still
write the `Pg` name.

In a module with `#[model]` or `#[repository]`, use
`autumn_web::db::prelude::*`, not `diesel::prelude::*`. Diesel's prelude also
imports the blocking `RunQueryDsl`, and the generated code then fails with
E0034 ("multiple applicable items in scope").

The routing key is the tenant id, resolved exactly as for `ShardedDb`
(`ShardKeyOverride`, then the tenancy middleware, then `[tenancy]`
extraction). In a tenant fleet, `tenant_scoped` is belt and braces. In a slot
fleet several tenants share a file, so `tenant_scoped` is what keeps them
apart.

`CrossShard<R>`, `Shards::each_shard` and `across_tenants()` fan out over
**every database on disk**, opening each. Use them on admin paths, not hot
paths. For analytics, [query the lake](#query-the-lake) instead.

When a handler holds a `Db` past `database.slow_query_threshold`, Autumn
logs `database connection held past slow_query_threshold` with `held_ms`.
The time covers all work in the hold, not one statement. For a fan-out, take
`LazyDb`. Call `checkout()` after the fan-out.

## Provisioning, deletion, backup

A tenant fleet does not create a database for an unknown tenant: any client
could otherwise fill the disk with ids. Provision on signup:

```rust
#[post("/signup/{tenant}")]
async fn signup(shards: Shards, Path(tenant): Path<String>) -> AutumnResult<StatusCode> {
    let fleet = shards.fleet().expect("fleet configured");
    fleet.provision(&fleet.key_for(&tenant)?).await?; // 409 when it exists
    Ok(StatusCode::CREATED)
}
```

| Call | Does |
| --- | --- |
| `fleet.provision(&key)` | create and migrate (`409` when it exists) |
| `fleet.open(&key)` | open (`404` when missing and not created on demand) |
| `fleet.delete(&key)` | close, then remove the file, its sidecars and the emptied bucket; requests get `410` meanwhile; `409` when another process still has it open. With replication, the replica goes too: see [Deleting a database's replica](#deleting-a-databases-replica) |
| `fleet.backup(&key, &dest)` | consistent copy with `VACUUM INTO`; writers keep running |
| `fleet.import(&key, &src)` | copy a file into the fleet as `key`: check, migrate, publish (`409` when the key has a file, `422` for a bad file). See [Import a backup](#import-a-backup) |
| `fleet.list()` | every database on disk |
| `fleet.each(n, f)` | run `f` on every database, `n` at a time |
| `fleet.migrate_all(n)` | migrate every database, report failures per database |
| `fleet.restore(&key, at)` | rebuild from the replica (needs replication); `409` when another process still has it open |
| `fleet.replicated_through(&key)` | the instant up to which every commit is in the replica. See [Know when a write is in the replica](#know-when-a-write-is-in-the-replica) |
| `fleet.wait_replicated(&key, at, timeout)` | wait until every commit at or before `at` is in the replica |
| `fleet.restore_from_file(&key, &src, opts)` | replace (or create) the database with a local backup; no replication needed. See [Restore a backup in place](#restore-a-backup-in-place) |
| `fleet.stats()` | counters (also on `/actuator/health` as `db:fleet`) |

### Import a backup

`fleet.import(&key, &src)` copies a database file into the fleet as `key`,
for example a file that `fleet.backup` wrote before a delete:

```rust
fleet.backup(&key, &archive).await?;
fleet.delete(&key).await?;
// Later: undo the delete.
fleet.import(&key, &archive).await?;
```

The import does not change `src`. It copies `src` with `VACUUM INTO` into a
private staging file. Then it does these steps:

1. It runs `PRAGMA integrity_check`.
2. It reads the identity row. The row must record `key`. A file with no
   identity row (a file that no fleet made) is refused.
3. It applies the pending migrations. With ownership, it sets the epoch of
   the copy to this host's epoch. Without ownership, it removes the epoch.
4. With replication, it purges the tombstone and the old replica of a
   deleted key, as `provision` does. It does this only after steps 1 to 3
   pass. Thus a refused file keeps the replica.
5. It publishes the file in one step. It never replaces a file.

A refused source answers `422` (`FleetError::InvalidSource`), and a key that
has a file answers `409`. Nothing is published then. `fleet.import_file` does
the same steps, but does not open the database.

Import only files that you trust: the copy keeps the schema, the triggers and
the migration records of `src`. On Windows, the import opens `src` read-write,
so SQLite can apply a `-wal` or a hot journal to it.

### Restore a backup in place

`fleet.restore_from_file(&key, &src, opts)` puts a backup back over the live
database of `key`. It also creates the file when the key has none.
Replication is not needed:

```rust
use autumn_web::db::fleet::LocalRestore;

fleet.backup(&key, &archive).await?;
// Later: roll the tenant back to the backup.
let outcome = fleet.restore_from_file(&key, &archive, LocalRestore::new()).await?;
```

The steps:

1. A key with a tombstone is refused, unless `clear_tombstone` is set.
2. The fleet copies `src` into a staging file with `VACUUM INTO`. The copy
   must pass `PRAGMA integrity_check`. Its identity row must record `key`.
   A trigger on the identity or epoch table is refused.
3. The fleet applies pending migrations to the copy when opens apply them
   (`apply_migrations_on_open`, set by `database.auto_migrate`). Otherwise
   pending migrations refuse the restore.
4. With ownership, the copy gets the epoch of this host. Without ownership,
   the copy has no epoch.
5. The key goes out of service. Openers wait. The open database closes.
6. The copy goes in place of the old file in one step, under the path lock.
   The old `-wal` and `-shm` go.
7. With `clear_tombstone`, the old replica and the tombstone go.
8. Openers continue. They see the restored file.
9. With replication, the fleet opens the database and ships a new base
   snapshot. A later replica restore then finds the restored data.

Steps 2 to 4 do not touch the live file, so openers keep working while the
copy runs. An error before step 6 changes nothing. A refused file or key
answers `FleetError::Restore` (`503`). This includes a file that is missing,
is not a database, is corrupt, or records another key. Another process that
has the file open gives `409`, and nothing changes.

Each option makes the restore accept more. All are off by default:

| `LocalRestore` | Does |
| --- | --- |
| `allow_rekey(true)` | accept a backup of another key, and write `key` into its identity row. Use it to copy a tenant to a new key |
| `clear_tombstone(true)` | restore a deleted key that has a tombstone. After the swap, the old replica and the tombstone go, as with `provision`. This ends the undo window |
| `apply_migrations(true)` | apply pending migrations even when opens do not |

A deleted key that has a tombstone is refused by default. The tombstone
keeps the old replica for undo, and a restore from a file would end that.
Use `fleet.restore_latest(&key)` to undo the delete from the replica. Use
`clear_tombstone(true)` to restore the file in its place. Without
replication there are no tombstones: a deleted key is restored at once.

`LocalRestoreOutcome` tells what changed: `replaced` (a file was there),
`rekeyed_from`, `migrations_applied`, `tombstone_cleared`, `shipped` (the
ship of the new base snapshot, `None` without replication) and
`ship_error`. A failed ship does not fail the call: the file is in place,
and `ship_error` says why. Then the replica holds older data. Ship the key
before any replica restore: an open in a process that replicates it ships
a new base. A replica restore that runs between steps 8 and 9 brings back
the replica: run restores of one key one at a time.

`FleetLifecycle::restore_missing` is a lower-level hook. Each request-path
open of a missing file asks it, under the path lock, whatever
`[database.fleet] restore_missing` says. A background open does not ask. With
replication, an open of a deleted key does not ask. Use `import` or
`restore_from_file` for a local backup.

## Operating the fleet

`autumn fleet` runs these operations from the command line. Use it when the
app is stopped and when it runs. It reads `[database.fleet]` the same way the
app does. `delete`, `import`, `restore` and `lake` also read `[replication]`. The sources are
`autumn.toml`, the `[profile.<p>]` overlay, `.env.<profile>` and `AUTUMN_*`
overrides. Run it from the app directory. `--profile <p>` selects a profile.

The fleet API is in the SQLite build only. Install that build of the CLI:

```shell
cargo install autumn-cli --no-default-features --features sqlite
```

| Command | Does |
| --- | --- |
| `autumn fleet list [--open] [--json]` | name, size and path of each database; `--open` adds the identity row, the latest applied app migration and the number of pending app migrations |
| `autumn fleet migrate [--concurrency N] [--key K]` | apply `migrations/` and the framework tables to every database, or to one |
| `autumn fleet backup <key> <dest>` | copy one database with `VACUUM INTO`; `dest` must not exist |
| `autumn fleet import <key> <src>` | copy `src` into the fleet as `key`; the key must have no file |
| `autumn fleet delete <key> --force` | remove one database and its sidecars; with replication, its replica too |
| `autumn fleet restore <key> [--timestamp T] --force` | rebuild one database from its replica; `--timestamp` sets a point in time |
| `autumn fleet restore <key> --from <file> --force` | replace one database with a local file; see [Restore a backup in place](#restore-a-backup-in-place) |
| `autumn fleet lake <sql> [--out F] [--keep F] [--timestamp T]` | run one query over the replicas of every live tenant and write CSV; see [Query the lake](#query-the-lake) |

```shell
autumn fleet list --open
autumn fleet migrate --concurrency 4
autumn fleet backup tenant:acme /backups/acme.db
autumn fleet import tenant:acme /backups/acme.db
autumn fleet delete tenant:acme --force
autumn fleet restore tenant:acme --timestamp 2026-10-01T12:00:00Z --force
autumn fleet restore tenant:acme --from /backups/acme.db --force
```

- A key is a name as `autumn fleet list` prints it: `tenant:<id>` or
  `slot:<nnnnn>`. `tenant/<id>` and `slot/<n>` are also accepted.
- No command creates the fleet root. Each command stops when
  `database.fleet.root` does not exist, so a wrong directory does not give an
  empty fleet. `lake` does not read the root, so it runs without one. For a new volume, create the root directory first.
- Only `restore` and `import` create a database file. The other commands do
  not.
- `import` applies `migrations/` and the framework tables to the copy before
  it publishes it. Like `migrate`, it stops when the copy records a migration
  that is not in these sets. With `[replication]` on, it needs the replica
  destination and the control database, to purge the old replica of a deleted
  database. With `[database.fleet.ownership]` on, it stops: this command holds
  no slot lease. Call `DatabaseFleet::import` in the app that owns the slot.
- Like boot, each command validates `[database]` first and stops when the
  control database resolves inside the fleet root.
- `list --open` reads each file and changes no data. It exits non-zero when
  it cannot read a file.
- `migrate` reads the app's `migrations/` directory. It does not apply plugin
  migrations. For those, run the app once with `AUTUMN_MIGRATE=1`. If an app
  migration has the same version as a framework migration, `migrate` stops:
  only boot can record that pair correctly. `migrate` also stops when a
  database records a migration that is not in `migrations/` or the framework
  sets (a plugin migration, for example). It checks each database again
  inside the migration's write lock, so a file replaced after the first check
  is not migrated. After you add a plugin, run the app once
  with `AUTUMN_MIGRATE=1` before you use `fleet migrate`: until then, no
  database records the plugin's migrations, so `migrate` cannot see them.
- A failure on one database does not stop the others. `migrate` lists each
  failure and exits non-zero.
- With `[replication]` on, a migration of a database that the app has open
  can make the replicator start a new generation. `AUTUMN_MIGRATE=1` does the
  same.
- `backup` never replaces or follows a file at `dest`. On Unix, only its
  owner can read the copy, and `backup` stops when another user could change
  a directory on the way to `dest`: each one must belong to root or to you,
  and must not be writable by others unless it has the sticky bit (as `/tmp`
  has). On Windows, the copy gets the ACL of the `dest`
  directory: put backups in a directory that only you can read.
- `delete` and `restore` destroy data, so they need `--force` on every
  profile. When another process has the file open (an app that serves the
  tenant), they stop with "open in another process" and change nothing. Wait
  until the app closes the database (`idle_close_secs`), or stop the app.
- `restore` verifies the replica chain before it replaces the file, as
  `autumn db replica restore` does.
- With `[replication]` on, `delete` also removes the replica, or keeps it for
  `replica_retention_after_delete`, and prints which. It needs the replica
  destination and the control database. If the purge fails, the command
  prints the error and a tombstone records it: the app tries the purge
  again. See [Deleting a database's replica](#deleting-a-databases-replica).
- `restore` also brings back a deleted database whose replica is still
  there. It drops the tombstone, so it needs the control database.
- `restore --from <file>` does not need `[replication]`. It applies
  `migrations/` and the framework tables to the copy, as `import` does.
  `--allow-rekey` accepts a backup of another key. `--clear-tombstone`
  restores a deleted key and purges its old replica. With `[replication]`
  on, it ships a new base snapshot of the restored file, so it needs the
  replica destination and the control database. With
  `[database.fleet.ownership]` on, it stops: this command holds no slot
  lease. On Unix, the file must belong to you or to root, and every
  directory on the way must pass the `backup` rule. A symlink is refused.
- On Windows, another process can open the database while `delete` or
  `restore` removes files (see [Limits](#limits)). The command then
  stops with "open in another process" or "created ... during the restore".
  The replica keeps the data. Stop the app, then run the command again.

## Migrations

Every fleet database gets the app's migrations and the shard framework tables
(version history, commit-hook queue, derivations). The control-plane tables
(tokens, jobs, sessions) stay on the control database.

- A new database is migrated when it is created.
- An existing database is migrated when it opens, if this boot applies
  migrations (`database.auto_migrate`, `auto_migrate_in_production`).
- Otherwise opening one with pending migrations answers `503` until you run
  the app once with `AUTUMN_MIGRATE=1`, which migrates the control database
  and then every fleet database. `autumn fleet migrate` migrates the fleet
  databases only (see [Operating the fleet](#operating-the-fleet)).

Each database's run is one `BEGIN IMMEDIATE` transaction: atomic, and safe
when two processes race. During a rollout the fleet is mixed, so write
migrations expand-then-contract.

### Control-only and tenant-only sets

`AppBuilder::migrations` sends a set to the control database and to every
fleet database. To keep a set on one side, use `migrations_for`, or
`plugin_migrations_for` in a plugin:

```rust
use autumn_web::migrate::{EmbeddedMigrations, MigrationScope, embed_migrations};

const SHARED: EmbeddedMigrations = embed_migrations!("migrations");
const CONTROL: EmbeddedMigrations = embed_migrations!("migrations-control");
const TENANT: EmbeddedMigrations = embed_migrations!("migrations-shards");

autumn_web::app()
    .migrations(SHARED)                               // every database
    .migrations_for(MigrationScope::Control, CONTROL) // control database only
    .migrations_for(MigrationScope::Shards, TENANT)   // each fleet database only
```

Boot and `AUTUMN_MIGRATE=1` obey the scope. The framework control-plane set
(`FRAMEWORK_MIGRATIONS`) always applies as `Control`.

The CLI reads the scope from the directory name
(`MigrationScope::directory`). Embed each directory with its scope, as
above:

| Directory | Scope | `autumn migrate` | `autumn fleet migrate` |
| --- | --- | --- | --- |
| `migrations/` | `Everywhere` | control database | each fleet database |
| `migrations-control/` | `Control` | control database | no |
| `migrations-shards/` | `Shards` | no | each fleet database |

With no `[database.fleet]` and no `[[database.shards]]`, the one database
gets every scope, at boot and from `autumn migrate`. Thus a plugin can
register `Control` or `Shards` and not check the topology.

`autumn migrate` applies the scope directories to a SQLite database only.
On a Postgres target that takes one, it stops: run the app once with
`AUTUMN_MIGRATE=1`. `autumn migrate check` scans all three directories.
`autumn migrate down` reads `migrations/` only, so it stops when a scope
directory exists. Plugin sets are not on disk: `AUTUMN_MIGRATE=1` applies
them.

## Opening and closing

- The first request for a database opens it (about half a millisecond for a
  small schema). Concurrent first requests share that one open.
- A file that records another key's name (it was copied or renamed by hand)
  is refused with `500` rather than served.
- `max_open` is a soft limit. Past it, the least recently used idle database
  closes. The sweep does not close a database that has a connection checked
  out, that a request holds (a `ShardedDb`, a sharded repository), or that
  was used within `min_residency` (default `2s`). An open never waits for
  room. Thus a burst across many keys opens more databases than `max_open`.
  In one measurement, 1,000 tenants at about 1,300 requests per second gave
  594 open databases with `max_open = 16`. The count decreased to 16 about
  12 seconds after the burst.
- A lower `min_residency` keeps the count nearer to `max_open` during a burst.
  A request can then reopen a database that it used a moment before.
- `max_resident_bytes` is a soft memory budget, with the same rules as
  `max_open`. The `SQLite` engine counts about 2 MB of page cache for each
  open connection. Above the budget, the least recently used idle database
  closes. The fleet keeps one database open, also when it alone is above
  the budget. `0` (the default) turns the budget off. `/actuator/health`
  reports `resident_bytes` under `db:fleet`.
- A new database is built and migrated in a private staging file, then
  published in one step: it is absent or complete, never half made. When two
  processes on one volume create the same database, both end up on one file.
- Budget: `pool_size` connections and about `2 × pool_size + 1` file
  descriptors (database and `-wal` per connection, one `-shm`) per open
  database. Size the open-file limit (`ulimit -n`) for the peak open count
  under a burst, not for `max_open`. The default limit on macOS is 256: 594
  open databases go past it.
- When connections reach `database.max_connections_warn_threshold`
  (default 100), the boot warning gives the open files at `max_open`. The
  formula is `2 × connections + databases`. It counts the control database.
- On Unix, boot also warns when those files reach 80 % of the open-file limit
  (`ulimit -n`), at any connection count. The rest is for sockets and logs.
  `database.max_connections_warn_threshold = 0` silences both warnings.
- Metrics label every fleet database `shard=fleet`; the database name goes on
  spans and logs only, so a tenant fleet cannot explode metric cardinality.

## Background work: the sweeper

Outbox rows, commit hooks and derivation backfills work on fleet databases.
A database does not have to be open for its work to run: the fleet sweeper
visits it.

```rust
#[post("/orders/{id}")]
async fn place(mut db: ShardedDb, outbox: Outbox, Path(id): Path<i64>) -> AutumnResult<()> {
    autumn_web::db::scoped_transaction(&mut *db, |conn| async move {
        outbox.write(conn, &format!("order:{id}"), "order.placed", &id).await?;
        Ok(())
    }.scope_boxed()).await
}
```

- The write also adds a row for the database to `_autumn_fleet_pending` on
  the control database, in the same transaction. A commit-hook enqueue does
  the same. If the control database cannot take the row, the write fails.
- A worker process (`role = "worker"` or `"combined"`) reads that table each
  second. It opens each listed database, sends its outbox, drains its hooks
  and runs its derivation backfill. Then it closes the database again, if
  nobody else used it.
- The sweeper never opens a database past `max_open`. With no room, it waits
  for the next pass. It never closes a database that a request holds, and
  it never restores a deleted database from its replica.
- An outbox handler for a tenant fleet runs in the tenant's scope
  (`CURRENT_TENANT`), so a `tenant_scoped` repository finds that tenant.
- A `web` process does not open databases for the sweeper. It drains the
  hooks of the databases it has open. A worker sends their outbox.
- A new or changed `#[derivation]` is backfilled in every database: the
  first worker pass after the deploy marks each file. When a database opens
  first, it runs up to 8 backfill batches before its first request, and the
  sweeper does the rest. During a rolling deploy, only the new binary runs
  backfills. After a rollback to older derivations, run a `resweep`.
- Write outbox rows in a transaction. Do not enqueue on a fleet database
  while you hold a write transaction on the control database: the row on the
  control database then cannot be written, and the write fails.

`/actuator/health` reports `fleet:sweeper`: `backlog` (databases with work),
`oldest_pending_secs`, and `last_pass` (`visited`, `cleared`, `deferred`,
`failed`, `handled`). A `backlog` that grows, or an old
`oldest_pending_secs`, means the sweeper cannot keep up: a handler fails, the
fleet is full, or no worker process runs.

## Replication: object storage between volumes

A cloud host attaches one volume. The volume can't be the unit of
durability or of moving data, so object storage is. With
[`[replication]`](sqlite-in-production.md#durability-continuous-replication-and-point-in-time-restore)
on, each open fleet database ships its WAL to the same destination as the
control database, under `<prefix>/<profile>/fleet/tenant/<id>` or
`.../fleet/slot/<nnnnn>`:

- fleet databases run with `wal_autocheckpoint = 0`, and their replicator is
  the only checkpointer (a fleet without replication keeps SQLite's own);
- closing a database ships everything it committed before the file closes.
  If the destination is down, the replicator stays open and keeps shipping
  until it catches up (`closing` in the health details);
- `fleet.restore(&key, at)` rebuilds one database, point in time included;
- `restore_missing = true` restores a database whose file is missing before
  opening it. A host with an empty volume takes over a tenant or a slot range
  by serving it. Turn on [slot ownership](#failover-slot-ownership) with it:
  without ownership, nothing stops the previous owner, and the two copies
  diverge. Boot logs a warning for `restore_missing` without ownership.
  The restored database holds what the previous owner shipped, and no more.
  A write it committed after its last sync is not in the replica, even if the
  request already returned success. So a takeover after a crash loses the
  writes since the last sync: up to `rpo_secs` while replication keeps up
  ([the RPO contract](sqlite-in-production.md#the-rpo-contract), and
  [Replication lag](#replication-lag)). Replication bounds that loss. It does
  not prevent it.
  This setting is only for a missing file. With ownership on, a host also
  restores a stale file (older than the replica) without `restore_missing`.
  See step 5 of [the failover procedure](#the-failover-procedure).

**Replication is asynchronous.** A host acknowledges a write when the write
is on its own volume. The next pass ships it to the replica. So a failover
to another volume can lose an acknowledged write. See
[What a failover costs](#what-a-failover-costs).

**Processes can share a replicated fleet root.** Only one process replicates a
database. It holds the lock file `<db>.replicator.lock`. The other processes
open the database and retry the lock on each tick. When the owner exits or
dies, the OS frees the lock and another process takes over with a new
generation. Use a local file system: advisory locks are not reliable on network
file systems.

`/actuator/health` reports `replication:fleet` with the number of databases
replicating, the number this process does not own (`unowned`), the worst lag
and which database has it.

### Replication lag

The `[replication] rpo_secs` bound holds while one pass over the open
databases fits in `sync_interval_secs`. A pass ticks each open database once.
It starts one interval after the last pass started, or at once if the last
pass took longer. `replication_workers` threads (default `4`, `1` to `64`)
ship databases at the same time. The database that shipped longest ago goes
first.

Many busy databases in one process make a pass long. Monitor these values in
`replication:fleet`:

- `worst_lag_seconds`: the longest time since a database was last caught
  up. A takeover can lose this much of that database. The indicator is
  `DOWN` above three times `rpo_secs`.
- `last_pass_ms`: the time of the last pass. If it is near
  `sync_interval_secs × 1000`, increase `replication_workers`, or put fewer
  busy databases on each host.
- `ship_workers`: the threads that ship.

`unowned` counts open databases that this process does not replicate.
Usually another process on the same root holds their lock. A database that
closes and opens again during a pass can show as `unowned` for some
milliseconds. The next pass claims it.

### Know when a write is in the replica

A commit is on the local disk at once. It is in the replica only after a
pass ships it. A takeover restores from the replica. So "committed" and
"survives a takeover" are not the same.

`fleet.replicated_through(&key)` gives an instant. Every transaction
committed to that database at or before that instant is in the replica.
Read the commit time from the app clock after the commit returns. Compare
it with the instant.

Use it for a write that spans two databases. For example, a row moves from
`src` to `dst`: upsert it into `dst`, then delete it from `src`. If the
delete ships and the upsert does not, a takeover loses the row. So delete
only when the upsert is in the replica:

```rust,ignore
upsert(&dst_db, &row).await?;
let committed_at = state.clock().now();
fleet
    .wait_replicated(&dst, committed_at, Duration::from_secs(5))
    .await?;
delete(&src_db, row.id).await?;
```

`replicated_through` is `None` when:

- the database is not open in this process, or another process replicates
  it;
- a newer owner fenced the replica;
- no tick proved the replica caught up since the current generation opened.

`wait_replicated` returns once the instant is covered. It fails with
`NotReplicating` (not open here), `Fenced`, or `TimedOut`. A timeout does
not mean the write is lost. It is not known yet: keep the source row and
try again later.

Rules:

- Use the clock the fleet was built with: the app clock. The bound assumes
  a clock that does not step back.
- In one generation the bound never moves back. A new generation or a new
  epoch starts it again.
- A closed database that still ships its backlog (`closing`) answers until
  it catches up.
- In `generations` mode the bound is the instant before the last snapshot
  that shipped. `rebuild` mode has no bound.
- The bound does not change `worst_lag_seconds`. Use the lag for alerts and
  the bound for a decision about one write.

### Deleting a database's replica

`fleet.delete(&key)` and `autumn fleet delete` remove the replica too:

1. The delete writes a tombstone row to `_autumn_fleet_tombstones` on the
   control database. If it cannot write the row, the delete fails (`503`)
   and removes nothing.
2. The delete removes the files.
3. The delete writes a marker object, `<prefix>/tombstone`, in the replica.
4. The delete purges the replica, then drops the row. If
   `replica_retention_after_delete` is set, the delete keeps the replica
   until the retention ends.

The purge sends one delete request for each object, and the delete call
waits for it. On a versioned bucket, a delete request only adds a delete
marker: add a lifecycle rule that removes old versions.

If the purge fails, the local delete is not rolled back. The row stays. A
task tries the purge again every minute. After each failure, it waits
longer, up to 1 hour. If the marker write failed, the task writes the
marker again. `replication:fleet` shows the last pass under `tombstones`
(`purged`, `failed`, `waiting`, `failing`, `last_error`). A failed purge
does not turn the indicator `Down`.

A tombstone stops the restore of a deleted database:

- `restore_missing` does not restore a key that has a row.
- On a host with its own control database, the marker stops the restore.
  The marker stops each generation that is not newer than the delete.
- `fleet.restore(&key, at)` and `autumn fleet restore` ignore the
  tombstone. They take the row first, and drop the marker after the
  restore. If the restore fails, the row goes back.
- `fleet.restore_from_file` refuses a key that has a row, unless
  `clear_tombstone` is set. Then it takes the row before the swap, and
  puts it back if the swap fails. It purges the old replica after the new
  file is in place.

Use the retention as an undo window:

```toml
[database.fleet]
replica_retention_after_delete = "30d"  # default "0s": purge at once
```

Use a unit: a bare number is milliseconds. A value under one minute is
refused.

Rules:

- During the retention, a create on demand (`create_on_demand`) of the key
  answers `410`. With `restore_missing`, a host with its own control
  database also answers `410`: it reads the retention from the marker.
- A create on demand also answers `410` while the row is pending, with any
  retention. A pending row has no cutoff yet. The task confirms the row when
  it is older than 10 minutes, the file is not on disk, this host owns the
  slot, and the replica can be read. Until then, only `provision` makes the
  database again.
- `provision` makes a new database: it purges the old replica
  first and drops the row. This ends the undo window. If the purge fails,
  `provision` fails with `503`: try again.
- A purge removes each generation that is not newer than the delete. A newer
  generation is a new database with the same key, and it stays. The delete
  time is the later of the app clock and the newest generation, so a host
  clock that is ahead does not keep the deleted data.
- A host purges only a key whose slot it owns, and only when the file is not
  on its disk. A row next to a live file is old: the task drops it.

### Restore check

A replica that does not restore is not a backup. Every `[replication]
verify_interval_secs` (default 6 hours), each worker-role process restores
`restore_check_sample` random databases (default `1`) into scratch files
under `<root>/.fleet-locks/restore-check/`. The first check runs one
interval after start. For each database, the check runs
`PRAGMA integrity_check` and reads the identity row, then removes the file.
A failed check is tried once more. Scratch files older than one day (from a
process that stopped during a check) are removed. Each check needs free
space for the largest sampled database on the fleet volume.

`replication:fleet` shows the result under `restore_check` (`checked_at`,
`passed`, `unreplicated`, `failed`). A failed check turns the indicator
`Down` until a check passes. A database with no replica yet counts as
`unreplicated`, not as a failure. Set `restore_check_sample = 0` or
`verify_interval_secs = 0` to turn the check off.
`fleet.check_replica_sample(n)` runs one check now.

## Query the lake

The lake runs SQL across tenants on their replicas in object storage, not
on the volume (ADR 0032). It never opens a file under the fleet root. It
runs on a host with no volume.

```rust,ignore
let snapshot = fleet.lake()?.materialize().await?;
let rows = snapshot
    .query("SELECT _autumn_tenant, COUNT(*), SUM(total) FROM orders GROUP BY 1")
    .await?;
```

- `materialize()` restores each live tenant and copies its tables into one
  scratch SQLite file. Each table gets the column `_autumn_tenant`.
- `query(sql)` runs one `SELECT`, `WITH` or `VALUES` statement on a
  read-only connection. `FleetLake::query(sql)` does the load, the query
  and the cleanup in one call.
- Drop the snapshot to remove its scratch directory. `persist(path)` keeps
  a copy for `sqlite3`, DuckDB and other tools.
- `snapshot.tenants()` gives the generation and effective time of each
  tenant. `snapshot.skipped()` gives each tenant that is not loaded and why.

On a host with no fleet, build the lake from the destination:
`FleetLake::new(destination, "<prefix>/<profile>")`.

| Setting | Default | Does |
| --- | --- | --- |
| `with_tombstones(t)` | from the fleet | skip tenants with a tombstone row |
| `markers_only()` | off | load with no tombstone source; use the markers only |
| `as_of(t)` | latest | each tenant at its newest state at or before `t` |
| `with_tables([..])` | all | load only these tables |
| `with_workers(n)` | 4 | restore `n` tenants at the same time |
| `with_scratch_dir(p)` | system temp | parent of the scratch directory |
| `skip_failed_tenants()` | off | report a failed tenant and load the rest |

Rules:

- **Consistency.** Each tenant is consistent at its own snapshot. The lake
  has no atomic snapshot across tenants. `as_of(t)` does not make one
  common instant either.
- **Deleted tenants.** A tombstone row, or a marker that covers the newest
  generation, skips the tenant. A lake with no tombstone source refuses to
  load unless you call `markers_only()`.
- **Failures.** A tenant that fails fails the load, so a total is never
  silently short.
- **Schema drift.** Columns union by name. A column that a tenant does not
  have is `NULL`. Values keep their storage class. Lake columns have no
  type affinity and use the `BINARY` collation, so write `COLLATE NOCASE`
  in the query where the source column had it.
- **Disk.** The lake needs about the size of the loaded tables, plus one
  restored file per worker. Use `with_tables` to load less.
- **Trust.** The SQL is operator SQL. Do not pass request input to it.

```shell
autumn fleet lake report.sql --out report.csv
autumn fleet lake report.sql --timestamp 2026-10-01T00:00:00Z --keep lake.db
```

The CLI uses the control database when its file is on the host. Else pass
`--markers-only`. It writes CSV: `NULL` is empty, a blob is hex. Slot fleets
are not supported.

### Lake sinks

The `SQLite` lake is one `LakeSink` (ADR 0040 §6). A plugin can give
another, for example a Parquet sink. Core keeps the inclusion rule. The sink
builds the lake.

```rust,ignore
let load = fleet.lake()?.load(MySink::open(..)).await?;
// load.lake: what the sink gave. load.members, load.skipped: the report.
```

- Core calls the sink in two passes, each in key order. First it calls
  `has(key, generation)` for each member that the inclusion rule takes.
  When it is `true`, core calls `keep(key, generation)` and restores
  nothing. Then core restores each other member and calls `add(member)`.
- `add` gets the `key`, the `generation` and the restored `file`. Core
  checks that the identity row of the file names the key. Core removes
  the file after `add`.
- A `LakeGeneration` holds the generation id and the WAL position. So a
  new commit in the same generation is a change.
- `finish()` gives the lake. A sink that keeps its lake between loads
  drops, at `finish`, each member that it got from neither `add` nor
  `keep`. A deleted tenant is gone then. So is a tenant that failed in
  this load.
- Set `TAKES_SLOTS = true` to get slot databases. One slot database holds
  many tenants, so its key names no tenant.
- `with_tables` applies to the `SQLite` lake only.
- Keep the sink's catalog in files under its own prefix, for example
  `<base>/lake/<sink>/`. Do not write under `<base>/fleet/`. Core adds no
  lake table to the control database.
- Return `LakeError::Ingest` for a member that fails. Leave no part of
  that member. `skip_failed_tenants()` then skips the member.

## Failover: slot ownership

With `[database.fleet.ownership]`, a host serves only the slot ranges that it
owns. Each claim of a range gets an **epoch**. An epoch is a number that only
increases. The epoch fences the old owner:

- the old owner stops opening the range's databases;
- its replicator stops;
- a restore never selects its data instead of the new owner's data.

```toml
[database.fleet]
restore_missing = true

[database.fleet.ownership]
owner = "host-a"     # one id per volume
slots = ["0-8191"]   # default: every slot, as one range
lease_secs = 30      # default; 3 to 3600
```

The lease also sets the takeover time after a crash. See
[What a failover costs](#what-a-failover-costs) before you change it.

The overrides are `AUTUMN_DATABASE__FLEET__OWNERSHIP__OWNER`, `..._SLOTS`
(comma-separated) and `..._LEASE_SECS`. The processes on one volume (a `web`
and a `worker` role) use the same `owner` and the same `slots`. A tenant fleet
uses the slot of the tenant id, so the same ranges work in both modes and in
the [cell router](cell-isolation.md).

### How it works

| Step | Rule |
| --- | --- |
| Claim | At boot, before the first request. If the range is free, or each lease on it ended, the host gets a new epoch. If the host already has the same range, it keeps its epoch. A live lease on any slot of the range refuses the claim, also a lease of the same owner on a range of another shape. |
| Renew | Every `lease_secs / 3`. The host renews all ranges at the same time. Each call has `lease_secs / 3` to answer. A slow range fails alone; the other ranges keep their leases. A slow call keeps running, so the range makes no new call until it ends. If the row has another owner or epoch, the host loses the range. |
| Serve | `open` checks the slot first. If the host does not own the slot, `open` returns `421 Misdirected Request` and does not touch the disk. So `restore_missing` runs only for an owned key. |
| Stamp | Each file records the newest epoch it was opened under. An open under an older epoch is refused with `421` (`NewerEpoch`), and the file does not change. The check and the stamp are one transaction. |
| Lose | The host closes every open database of the range. The close does not wait for workers, but it waits for connections that are in use. The sweeper and the fan-outs skip the range. |
| Lease length | A lease never ends earlier than an older call set it. If you make `lease_secs` shorter, the new length applies when the old lease ends. |
| Local lease | A host that cannot renew stops serving `2/3 × lease_secs` after it sent its last renew. This is before the lease in the table ends. |
| Replicas | Each generation records its epoch. A restore selects the newest epoch, then the newest time. Before each write to the replica, core reads the epochs at the destination: one list for each put. If it finds a newer epoch, the write is refused, the replicator stops, and the host loses the range. |
| Shutdown | The host closes its databases. Then the last process on the volume releases the ranges, if nothing is left unshipped: no parked WAL replicator, and no closed database whose last generation did not ship. The next owner can then claim them at once. In all other cases, the leases end on their own. |

The ownership table is `_autumn_fleet_slot_owners` on the control database.
All hosts of the fleet must use the same table. The control database is a
local SQLite file, so the built-in table fences only the processes of one
host. For hosts on different machines, give boot a store that all hosts
reach:

```rust,ignore
use autumn_web::db::fleet_ownership::PgSlotOwnership;

autumn_web::app()
    .fleet_ownership_store(PgSlotOwnership::new(pg_pool, clock))
    .run()
    .await;
```

`PgSlotOwnership` keeps the table in Postgres. It needs the `postgres`
feature next to `sqlite`. Each call is one transaction that locks the table,
so two hosts that race a claim get one owner. Boot calls `ensure_schema` on
the store before the first claim. You can also implement
`SlotOwnershipStore` (in `autumn_web::db::fleet_ownership`) on another
store. One store holds the ownership of one fleet. Do not give two fleets
one store: the release of one fleet ends the lease of the other.

### The failover procedure

1. Give every host the same `[replication]` destination and the same
   ownership table. Set `restore_missing = true` and a unique `owner` per
   volume.
2. **Planned move.** Stop the old host. It releases its ranges at shutdown.
   Start the new host with the same `slots`. It claims the ranges at once and
   restores each database on its first request.
3. **Unplanned failover.** The old host stopped, or the network isolates it.
   Start the new host with the same `slots`. The new host claims the ranges
   when the old lease ends: after at most `lease_secs`, plus one renew
   interval and the clock skew. Then it serves at a new epoch.
   The new host serves what the old host shipped to the replica. Writes that
   the old host committed in its last `rpo_secs` can be missing, including
   writes it answered with success. They stay in the old host's files, but a
   fail-back (step 5) replaces those files with the replica, so they do not
   come back. A planned move (step 2) loses nothing: the old host ships
   everything before it releases the ranges.
   A write that spans two databases can be lost in half. To prevent this,
   do the second half only when the first is in the replica. See
   [Know when a write is in the replica](#know-when-a-write-is-in-the-replica).
4. If the old host is still alive, it stops serving when its local lease
   ends, and returns `421`. When it can reach the table again, its renew
   fails. It claims the range again only when the new owner lets it go, at a
   newer epoch. Until then, `fleet:ownership` shows the range as `waiting`.
5. **Fail back.** When the old host gets a range back, its files on disk are
   older than the replica. Each file records the epoch it was last opened
   under. On the first open at the newer epoch, the host compares that epoch
   with the newest epoch at the replica. If the replica's is newer, the host
   restores the file from the replica before it serves it. This works
   without `restore_missing`. Without `[replication]` there is nothing to
   compare with: give the old host an empty volume.
6. **Move part of a range.** Change `slots` on the owner, for example from
   `"0-8191"` to `"0-4095"`, and restart it. A clean shutdown releases the
   old range, so the owner claims `"0-4095"` at once at a new epoch. Another
   host can then claim `"4096-8191"`. If the old range was not released, each
   claim waits until its lease ends.

`/actuator/health` reports `fleet:ownership`. The details are the `owner`,
the `held` ranges with their epoch and whether the host serves them now, and
the ranges that are `waiting`, `lost` or `failed`. The status is `Down` when
the last claim or renew could not reach the table, or when a held range
stopped serving because a renew did not come back in time.

### What a failover costs

Replication is asynchronous (see
[Replication](#replication-object-storage-between-volumes)). A failover
costs data and time:

| Cost | Bound | Default |
| --- | --- | --- |
| Data | The writes that the old host did not ship. In steady state, at most `rpo_secs` (see [Replication lag](#replication-lag)). At worst, the last `worst_lag_seconds` in `replication:fleet` before the failover. | 10 s (`rpo_secs`) |
| Time, old host crashed | At most `lease_secs + lease_secs / 3` after the last renew of the old host, plus the clock skew and the first restore. The lease ends, then the waiting host claims at its next renew interval (every `lease_secs / 3`). | 40 s |
| Time, old lease already ended | At most `lease_secs / 3`: the next renew interval of the waiting host. | 10 s |

- A long lease gives a long outage after a crash. A short lease allows less
  clock skew (see [Clocks](#clocks)).
- A crashed host loses all the writes that it did not ship.
- A paused host (`SIGSTOP`, a frozen VM) loses the unshipped writes of each
  database that the new owner opened during the pause. When the paused host
  continues, it ships its other writes. The new owner restores them later.
- A fail-back does not recover the lost writes. If the new owner shipped a
  database, the old host restores the replica over its stale file (step 5).
  If not, the old host keeps its own file.

### Clocks

The lease in the table uses the wall clock of the host that writes it. The
holder stops on its own monotonic clock, `lease_secs / 3` before the lease
ends. The clock skew between hosts plus the longest request must be less
than `lease_secs / 3` (10 s by default). Otherwise two hosts can serve one
slot for a short time. The epoch still protects the replicas: a restore
selects the newest epoch, and the old replicator stops.

If the old owner accepts writes after the new owner claims the range, those
writes are lost. They are in the old epoch's generation, and no restore
selects that generation.

Do not restore the control database to an earlier time while ownership is
on. If its epoch counter goes back, the next claim after a fence still goes
above the epoch that the replicator found.

## Memory-mapped reads

`database.fleet.mmap_size` and `database.fleet.cache_size_kib` tune the
fleet databases. They are separate from `database.mmap_size` and
`database.cache_size_kib`, which tune only the control database. All four are
off by default.

For most fleets, keep these values at 0. A fleet file is small. SQLite keeps
its pages in its cache. A fleet of 384 small files saw no change with
mmap on.

Turn on `mmap_size` only for a fleet of large, read-heavy files. Read
[Memory-mapped reads](./sqlite-in-production.md#memory-mapped-reads-and-the-page-cache)
first: an I/O error on a mapped file stops the process with `SIGBUS`.

Each open database maps its file once for each connection. The worst case is
`mmap_size` × `max_open` × `pool_size`. `max_open` is a soft limit, so a burst
can map more. On a 32-bit target, config validation refuses a worst case over
1 GiB, with the control database included.

Replication, `restore_missing`, LRU close and reopen work with mmap on. A
close drops the pool, and its connections unmap the file.
## Writing a fleet extractor

A plugin that runs a fleet of another engine (DuckDB, RocksDB) writes a
`FleetEngine` and one extractor. The extractor calls
`autumn_web::fleet::resolve`. That function runs these steps:

1. It finds the routing key: a `ShardKeyOverride`, then the tenant.
2. It makes the database key from the routing key.
3. It checks that this host owns the slot. If not, it returns `421`
   (`NotOwner`) and does not touch the disk.
4. It opens the database. If the fleet creates on demand, it makes a
   missing database.

It returns a `FleetLease`. While the lease lives, the fleet does not close
the database. The lease drops at the end of the handler, so the fleet can
then evict the database.

```rust,ignore
use autumn_web::fleet::FleetLease;

pub struct FleetDuckDb(FleetLease<DuckDbEngine>);

impl FromRequestParts<AppState> for FleetDuckDb {
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, AutumnError> {
        autumn_web::fleet::resolve::<DuckDbEngine>(parts, state)
            .await
            .map(Self)
            .map_err(AutumnError::from)
    }
}
```

Core has no generic extractor. Each plugin names its own and maps its own
errors.

- **The fleet.** `resolve::<E>` uses the `DatabaseFleet<E>` in the app
  state. Put it there with `state.insert_extension(fleet)`, for example in a
  `state_initializer`. Without it, `resolve` returns `503` (`NoFleet`). With
  `sqlite`, `resolve::<SqliteEngine>` also finds the fleet of
  `[database.fleet]`.
- **Jobs and tasks.** `resolve_key::<E>(state, tenant_id)` is the same path
  with no request.
- **No routing key.** A request with no tenant returns `400`
  (`NoRoutingKey`). An app state with no config returns `503`.
- **Ownership.** `autumn_web::fleet::ownership_store(state)` returns the store
  of `fleet_ownership_store`, else the SQL store of the control database.
  Give it to `FleetOwnership::new`, then call `DatabaseFleet::start_ownership`
  and `DatabaseFleet::spawn_ownership` before the first request.

### Sidecar engines

A sidecar engine follows the keys of another fleet. For example, it keeps a
DuckDB file next to each tenant's SQLite file. Install a `FleetLifecycle` on
that fleet:

- `on_open` and `on_close` get the `FleetDatabase`. `db.epoch()` is the
  ownership epoch the database opened under.
- `db.sidecar_replica("duckdb")` gives a `ScopedDestination` at
  `<key prefix>/duckdb/`, next to the primary's replica. It refuses every key
  outside that prefix. It is `None` when the fleet does not replicate.
- A sidecar name is 1 to 32 bytes of `a-z`, `0-9`, `_` and `-`. The names
  `generations`, `epochs` and `tombstone` are reserved.
- Put each copy under `generations/<id>/` of the scope. Make the id with
  `autumn_web::replication::segment::generation_id`.
- A purge after a delete removes the sidecar's objects. It keeps the
  generations of a newer database with the same key, and removes every
  other object of the scope.
- The scope refuses writes after the database closes, and after a fence, a
  delete or a restore. Each write also checks the epoch: once this host
  loses the slot, the scope refuses writes at once. Before each write, the
  scope also reads the epoch markers of the primary's replica. A newer
  marker refuses the write with `DestinationError::Fenced`. Ship the last
  files in `on_close`. Reads still work.

### Shipping a plugin engine

`FleetEngine::ship_mode` tells core how the databases get to object
storage. Core does every write to the replica. The engine only gives files.

| Mode | What core does | RPO |
| --- | --- | --- |
| `Continuous(_)` | Ships the changes as they commit. Only `SqliteEngine` (WAL shipping). `ContinuousShipper` is sealed. | `sync_interval` |
| `Generations { every }` | Takes a `snapshot` every `every` and at close. Ships only the files whose SHA-256 is not in the previous generation. | `every` |
| `Rebuild` | Ships nothing. `restore_latest` calls `FleetEngine::rebuild`. | none |

Install replication as for `SQLite`:
`fleet.install_replication(Arc::new(FleetReplication::new(..)))`. The mode
decides what it starts. Replication needs the `sqlite` feature: it keeps
the tombstones on the `SQLite` control database. `FleetStats` and the `replication:fleet` health
indicator report `ship_mode` and the RPO (`rpo_secs`, `rpo_seconds`).

A generation of `Generations` mode is these objects:

```text
<key prefix>/epochs/<epoch>                    the epochs that wrote here
<key prefix>/generations/<id>/epoch-<epoch>
<key prefix>/generations/<id>/files/<sha256>   files new in this generation
<key prefix>/generations/<id>/manifest.json    written last
```

- **Unchanged files.** The manifest names, for each file, the generation
  that holds its content. A file with no change is not shipped again. A
  snapshot with no change ships nothing. So a snapshot of unchanged data
  must be the same bytes.
- **Prune.** Core keeps the two newest generations, and each generation
  that their manifests name.
- **Fence.** Before each write, core reads the epoch markers. A newer
  epoch refuses the write (`FleetError::Fenced`, `421`), and this host
  loses the slot. This holds in every mode.
- **Restore.** A missing database with `restore_missing` takes the newest
  generation, by epoch and then by time. Core checks the SHA-256 and the
  length of each file. The tombstone rules apply as for `SQLite`: a delete
  blocks the restore.
- **Fail-back.** Return the stamp from `FleetEngine::epoch_stamp`. When the
  replica holds a newer epoch of another owner, core restores the database
  before the open. With `None`, core counts the stamp as 0: it restores at
  each new epoch that another owner wrote.
- **Epoch marker.** A new owner writes its epoch marker when it opens the
  database, before its first generation. From then on, each write of the
  old owner is refused.
- **Close.** The last generation ships at close. If that ship fails, or
  the shutdown cutoff passed, core keeps the database as unshipped work.
  A close after a lapse of this host's lease keeps it too. The
  replication loop (`FleetReplication::spawn_loop`) tries the ship again
  on each pass. The shutdown flush tries it within its budget. A ship
  that works drops it. A delete, a restore or a newer epoch also drops
  it. Then nothing ships for it. A delete or a restore that fails keeps
  it. While the database is open, the loop does not try it: a ship of the
  open database drops it. During the snapshot of a retry, opens wait.
  Until it ships, `release_ownership` lets the ranges lapse. The
  `replication:fleet` health details name it in `unshipped`.
- **Snapshot shape.** A snapshot is one database: one file, or one
  directory of files. Core refuses two paths that are the same.
- **Operators.** `DatabaseFleet::ship_now(key)` ships an open database now.
  `DatabaseFleet::restore_latest(key)` restores from the newest replica, in
  every mode. Within the retention, it undoes a delete.

`plugin_conformance::fleet` checks the engine side. `fleet-fenced-ship`
and `fleet-unchanged-files` need a fixture that installs replication.

## Limits

- Shard-local jobs and the job dashboard work on the control database, not
  on fleet databases.
- Each open database runs its own commit-hook worker, in every process role.
  Before a database closes (idle, `max_open`, shutdown), its worker
  finishes the hook it runs and acks it, for up to 10 seconds. A hook that
  does not finish in that time can run again later. Hooks stay
  at-least-once: write them to be idempotent.
- An outbox row written outside a transaction commits before the sweeper's
  row. If that row cannot be written, the call returns an error for a
  committed message: do not retry it. The next open of the database writes
  the row.
- A replicated database takes a fresh base snapshot each time it reopens.
  Size `max_open` so busy tenants stay open.
- On Windows, `delete` and `restore` close the guard connection before they
  remove files. If another process opens the database in that gap, `delete`
  answers `409`. `restore` also answers `409` if that process creates the
  file before `restore` publishes.

## With `autumn-harvest-sqlite`

`autumn-harvest-sqlite` and the `sqlite` feature of `autumn-web` use the same
`libsqlite3-sys`, so the two link one SQLite. `SqliteRuntime` and its drive
future are `Send`: one tokio task can own the runtime.

## See also

- [Horizontal Sharding](sharding.md): Postgres shards, slots, routers
- [SQLite in Production](sqlite-in-production.md): the single-database tier
- [Cell and Shuffle-Shard Isolation](cell-isolation.md): cells and the cell router
