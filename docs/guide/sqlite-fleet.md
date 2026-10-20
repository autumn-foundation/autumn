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
max_open = 256                  # open databases at most (LRU past this)
pool_size = 2                   # connections per open database
idle_close_secs = 300           # close a database idle this long
# create_on_demand = false      # default: false for tenants, true for slots
# restore_missing = false       # see "Replication" below

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
async fn list(repo: PgNoteRepository) -> AutumnResult<Json<Vec<Note>>> {
    Ok(Json(repo.find_all().await?))
}
```

The routing key is the tenant id, resolved exactly as for `ShardedDb`
(`ShardKeyOverride`, then the tenancy middleware, then `[tenancy]`
extraction). In a tenant fleet, `tenant_scoped` is belt and braces. In a slot
fleet several tenants share a file, so `tenant_scoped` is what keeps them
apart.

`CrossShard<R>`, `Shards::each_shard` and `across_tenants()` fan out over
**every database on disk**, opening each. Use them on admin paths, not hot
paths.

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
| `fleet.delete(&key)` | close, then remove the file, its sidecars and the emptied bucket; requests get `410` meanwhile; `409` when another process still has it open |
| `fleet.backup(&key, &dest)` | consistent copy with `VACUUM INTO`; writers keep running |
| `fleet.list()` | every database on disk |
| `fleet.each(n, f)` | run `f` on every database, `n` at a time |
| `fleet.migrate_all(n)` | migrate every database, report failures per database |
| `fleet.restore(&key, at)` | rebuild from the replica (needs replication); `409` when another process still has it open |
| `fleet.stats()` | counters (also on `/actuator/health` as `db:fleet`) |

## Operating the fleet

`autumn fleet` runs these operations from the command line. Use it when the
app is stopped and when it runs. It reads `[database.fleet]` the same way the
app does. `restore` also reads `[replication]`. The sources are
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
| `autumn fleet delete <key> --force` | remove one database and its sidecars |
| `autumn fleet restore <key> [--timestamp T] --force` | rebuild one database from its replica; `--timestamp` sets a point in time |

```shell
autumn fleet list --open
autumn fleet migrate --concurrency 4
autumn fleet backup tenant:acme /backups/acme.db
autumn fleet delete tenant:acme --force
autumn fleet restore tenant:acme --timestamp 2026-10-01T12:00:00Z --force
```

- A key is a name as `autumn fleet list` prints it: `tenant:<id>` or
  `slot:<nnnnn>`. `tenant/<id>` and `slot/<n>` are also accepted.
- No command creates the fleet root. Each command stops when
  `database.fleet.root` does not exist, so a wrong directory does not give an
  empty fleet. For a new volume, create the root directory first.
- Only `restore` creates a database file. The other commands do not.
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
- `delete` does not remove the replica. With `restore_missing = true`, the app
  restores a deleted database on the next request for it.
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

## Opening and closing

- The first request for a database opens it (about half a millisecond for a
  small schema). Concurrent first requests share that one open.
- A file that records another key's name (it was copied or renamed by hand)
  is refused with `500` rather than served.
- Past `max_open`, the least recently used idle database closes. A database
  with a connection checked out, used in the last two seconds, or held by a
  request (a `ShardedDb`, a sharded repository) is never closed, so the cap is
  soft under load.
- A new database is built and migrated in a private staging file, then
  published in one step: it is absent or complete, never half made. When two
  processes on one volume create the same database, both end up on one file.
- Budget: `max_open × pool_size` connections and roughly
  `max_open × (2 × pool_size + 1)` file descriptors.
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

**Processes can share a replicated fleet root.** Only one process replicates a
database. It holds the lock file `<db>.replicator.lock`. The other processes
open the database and retry the lock on each tick. When the owner exits or
dies, the OS frees the lock and another process takes over with a new
generation. Use a local file system: advisory locks are not reliable on network
file systems.

`/actuator/health` reports `replication:fleet` with the number of databases
replicating, the number this process does not own (`unowned`), the worst lag
and which database has it.

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

The overrides are `AUTUMN_DATABASE__FLEET__OWNERSHIP__OWNER`, `..._SLOTS`
(comma-separated) and `..._LEASE_SECS`. The processes on one volume (a `web`
and a `worker` role) use the same `owner` and the same `slots`. A tenant fleet
uses the slot of the tenant id, so the same ranges work in both modes and in
the [cell router](cell-isolation.md).

### How it works

| Step | Rule |
| --- | --- |
| Claim | At boot, before the first request. If the range is free, or each lease on it ended, the host gets a new epoch. If the host already has the same range, it keeps its epoch. A live lease on any slot of the range refuses the claim, also a lease of the same owner on a range of another shape. |
| Renew | Every `lease_secs / 3`. If the row has another owner or epoch, the host loses the range. |
| Serve | `open` checks the slot first. If the host does not own the slot, `open` returns `421 Misdirected Request` and does not touch the disk. So `restore_missing` runs only for an owned key. |
| Lose | The host closes every open database of the range. The close does not wait for workers, but it waits for connections that are in use. The sweeper and the fan-outs skip the range. |
| Local lease | A host that cannot renew stops serving `2/3 × lease_secs` after it sent its last renew. This is before the lease in the table ends. |
| Replicas | Each generation records its epoch. A restore selects the newest epoch, then the newest time. A replicator reads the epochs at the destination at each new generation and every 60 s. If it finds a newer epoch, it stops, and the host loses the range. |
| Shutdown | The host closes its databases. Then the last process on the volume releases the ranges, if nothing is left unshipped. The next owner can then claim them at once. In all other cases, the leases end on their own. |

The ownership table is `_autumn_fleet_slot_owners` on the control database.
All hosts of the fleet must use the same table. The control database is a
local SQLite file, so the built-in table fences only the processes of one
host. For hosts on different machines, implement `SlotOwnershipStore` (in
`autumn_web::db::fleet_ownership`) on a store that all hosts reach. Then
install a `FleetOwnership` with `DatabaseFleet::install_ownership`, and start
`DatabaseFleet::spawn_ownership` before the first request.

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
4. If the old host is still alive, it stops serving when its local lease
   ends, and returns `421`. When it can reach the table again, its renew
   fails. It does not serve the range again.
5. **Move part of a range.** Change `slots` on the owner, for example from
   `"0-8191"` to `"0-4095"`, and restart it. A clean shutdown releases the
   old range, so the owner claims `"0-4095"` at once at a new epoch. Another
   host can then claim `"4096-8191"`. If the old range was not released, each
   claim waits until its lease ends.

`/actuator/health` reports `fleet:ownership`. The details are the `owner`,
the `held` ranges with their epoch and whether the host serves them now, and
the ranges that are `waiting`, `lost` or `failed`. The status is `Down` when
the last claim or renew could not reach the table.

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
- A deleted database's replica stays in object storage until you remove it.
- On Windows, `delete` and `restore` close the guard connection before they
  remove files. If another process opens the database in that gap, `delete`
  answers `409`. `restore` also answers `409` if that process creates the
  file before `restore` publishes.

## See also

- [Horizontal Sharding](sharding.md): Postgres shards, slots, routers
- [SQLite in Production](sqlite-in-production.md): the single-database tier
- [Cell and Shuffle-Shard Isolation](cell-isolation.md): cells and the cell router
