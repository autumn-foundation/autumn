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
```

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
| `fleet.delete(&key)` | close, then remove the file, its sidecars and the emptied bucket; requests get `410` meanwhile |
| `fleet.backup(&key, &dest)` | consistent copy with `VACUUM INTO`; writers keep running |
| `fleet.list()` | every database on disk |
| `fleet.each(n, f)` | run `f` on every database, `n` at a time |
| `fleet.migrate_all(n)` | migrate every database, report failures per database |
| `fleet.restore(&key, at)` | rebuild from the replica (needs replication) |
| `fleet.stats()` | counters (also on `/actuator/health` as `db:fleet`) |

## Migrations

Every fleet database gets the app's migrations and the shard framework tables
(version history, commit-hook queue, derivations). The control-plane tables
(tokens, jobs, sessions) stay on the control database.

- A new database is migrated when it is created.
- An existing database is migrated when it opens, if this boot applies
  migrations (`database.auto_migrate`, `auto_migrate_in_production`).
- Otherwise opening one with pending migrations answers `503` until you run
  the app once with `AUTUMN_MIGRATE=1`, which migrates the control database
  and then every fleet database.

Each database's run is one `BEGIN IMMEDIATE` transaction: atomic, and safe
when two processes race. During a rollout the fleet is mixed, so write
migrations expand-then-contract.

## Opening and closing

- The first request for a database opens it (about half a millisecond for a
  small schema). Concurrent first requests share that one open.
- A file that records another key's name (it was copied or renamed by hand)
  is refused with `500` rather than served.
- Past `max_open`, the least recently used idle database closes. A database
  with a connection checked out, or used in the last two seconds, is never
  closed, so the cap is soft under load.
- A new database is built and migrated in a private staging file, then
  published in one step: it is absent or complete, never half made. When two
  processes on one volume create the same database, both end up on one file.
- Budget: `max_open × pool_size` connections and roughly
  `max_open × (2 × pool_size + 1)` file descriptors.
- Metrics label every fleet database `shard=fleet`; the database name goes on
  spans and logs only, so a tenant fleet cannot explode metric cardinality.

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
  by serving it.

**Only turn on `restore_missing` for keys the previous owner has stopped
writing**, or the two copies diverge. Ownership fencing is on the roadmap.

`/actuator/health` reports `replication:fleet` with the number of databases
replicating, the worst lag and which database has it.

## Limits

- The outbox relay, derivation backfill, shard-local jobs and the job
  dashboard work on the control database, not on fleet databases. Enqueue
  through the control database.
- Each open database runs its own commit-hook worker. Hooks queued in a
  database drain when it is open, and resume on its next open.
- A replicated database takes a fresh base snapshot each time it reopens.
  Size `max_open` so busy tenants stay open.
- A deleted database's replica stays in object storage until you remove it.

## See also

- [Horizontal Sharding](sharding.md): Postgres shards, slots, routers
- [SQLite in Production](sqlite-in-production.md): the single-database tier
- [Cell and Shuffle-Shard Isolation](cell-isolation.md): cells and the cell router
