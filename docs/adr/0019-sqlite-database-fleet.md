# ADR 0019: SQLite Database Fleet (one database per tenant or per slot)

- Status: Accepted
- Date: 2026-10-09
- Deciders: Autumn maintainers
- Tags: sqlite, sharding, multi-tenancy, replication, cells

## Context

Autumn's SQLite tier has two structural limits:

1. **One writer.** SQLite serializes writes per database. With every tenant in
   one file, every tenant's writes queue behind every other tenant's.
2. **One host, one volume.** A cloud host attaches one volume. The volume is
   the unit of failure, and nothing can move data between volumes except
   copying it.

Sharding (`autumn/src/sharding.rs`) already splits tenant data across
databases: a key hashes onto one of 16384 slots, and a slot maps to a shard.
But the shard list is fixed configuration, and SQLite shards were refused at
four layers (config validation, backend consistency, the boot guard, the
CLI). A fixed list cannot hold "one database per tenant": the tenant count is
unbounded, and 16384 per-slot files cannot be listed in `autumn.toml`.

### Prior art

| System | Unit | Routing | Open / close | Migrations | Lesson |
| --- | --- | --- | --- | --- | --- |
| Basecamp `activerecord-tenanted` | file per tenant | subdomain → `connected_to(shard:)` | lazy pools, LRU cap (50) | on create; refuse pending on open; `migrate_all` | Tenant-name validation misses `..`. Fizzy left per-tenant SQLite two days before launch over replication/failover of single-writer files. |
| Bluesky PDS | file per user | DID → `sha256` prefix dir | open per operation; never create on lookup | eager on create, bounded fan-out | Check the file exists before opening: diesel/SQLite create on open. |
| Turso / libSQL | DB per user (millions) | client picks DB | server-side | parent/child schema fan-out, now deprecated | Never lock the fleet for a migration; code must tolerate a mixed fleet. |
| Cloudflare Durable Objects | SQLite per object | name → object | idle eviction 70–140 s | in the constructor (lazy) | WAL streamed to followers; object storage is the durability layer. |
| Tailscale | tailnets bucketed into N SQLite shards | tailnet → shard | long-lived | — | Manual checkpointing hit the WAL-reset race fixed in SQLite 3.51.3. Autumn bundles 3.53.2. |
| Litestream 0.5 | one replica per DB, directory mode | — | holds a read lock | — | "Does not track database deletions": deleting a tenant must delete its replica explicitly. |
| Apple CloudKit (FDB Record Layer) | record store per (user, app) | — | — | version checked on open, built lazily | The best model for lazy, versioned migration at huge counts. |

Measured on this container: one idle connection costs about 190 KB RSS and
2 file descriptors (`.db`, `-wal`), plus one `-shm` per database per process.
Opening a database and running its first query takes about 0.5 ms with a
20-table schema.

## Decision

### 1. A fleet is a shard set whose shards open on demand

`[database.fleet]` (SQLite builds only) configures a `DatabaseFleet`. At boot
it becomes the app's `ShardSet` (`ShardSet::from_fleet`), so `ShardedDb`,
`ShardedReadDb`, `Shards`, `CrossShard` and `#[repository(sharded)]` route to
a fleet database with no change to app code. The top-level `database.url`
stays the control database: sessions, jobs, flags and other fleet-wide state
live there.

Two modes share one implementation:

| | `mode = "tenant"` | `mode = "slot"` |
| --- | --- | --- |
| File per | tenant | routing slot (16384 at most) |
| Default path | `{bucket}/{tenant}.db` | `{bucket}/slot-{slot}.db` |
| Isolation | hard; a tenant's data is one file | `tenant_scoped` filters within a slot |
| Created on first use | no (provision explicitly) | yes |
| Unit that moves between hosts | tenant | slot (or a `{bucket}` of 64 slots) |

`{bucket}` is `slot / 64`, so a bucket is a contiguous slot range: a host that
owns slots 0–8191 owns buckets 000–127, and a cell handoff is a directory.

`ShardSet::route` returns a borrow into a fixed list, which a database opened
on demand cannot satisfy. The new `ShardSet::resolve` returns an owned
(`Arc`-backed) `Shard`, and every framework extractor now resolves through it.
`route` keeps its signature and refuses on a fleet-backed set.

### 2. Tenant ids are file names, so they are validated as file names

The tenancy layer only rejects empty ids. A fleet admits `[a-z0-9_-]`, starting
with a letter or digit, at most 128 bytes, never a Windows device name.
Uppercase is refused, not folded: on a case-insensitive file system `Acme` and
`acme` would share a file. A refused id is `400`, and nothing is created.

### 3. Lazy open, single flight, bounded and safe eviction

- A key's first opener builds the pool, records the key's name in
  `_autumn_fleet_identity`, and migrates. Concurrent openers wait on the same
  `OnceCell`.
- A file whose identity row names another key is refused (`500`). That catches
  a database copied or renamed by hand, which would otherwise serve one
  tenant another's data.
- Past `max_open`, the least recently used *idle* database closes; past
  `idle_close_secs`, any idle database closes. A database with a checked-out
  connection is never chosen, nor one used in the last two seconds, nor one
  somebody holds a lease on. A `Shard` resolved for a request carries a
  `ShardLease`, and generated repositories keep it for their lifetime, so a
  connection acquired lazily — long after the database was resolved — never
  finds its pool closed. The sweeper re-checks `max_open` at least once per grace
  window, so a burst of opens does not stay over the cap.
- A new database is built in a private staging file beside its path —
  identity row and every migration — and published with a hard link, which
  refuses an existing target. A database is therefore absent or complete,
  never half made; a failed creation removes only its own staging file; and
  when two processes (say a `web` and a `worker` role on one volume) create
  the same database, the second opens the first one's file instead of
  deleting it.
- Several processes may share one fleet root (a `web` and a `worker` role on
  one volume). Delete and restore therefore prove no other process has the
  file open before touching it: a guard connection switches the database out
  of WAL under `locking_mode = EXCLUSIVE` with no busy wait, which `SQLite`
  refuses while any other connection has the file open. A refusal is `409`;
  a success also keeps new openers out until the files are replaced.
- Keys are checked against the fleet (mode, slot range, a tenant's own slot)
  before anything touches the disk.
- Closing calls `Pool::close` (a stale handle can no longer check out), waits
  for in-flight connections, then runs lifecycle hooks. An opener for a
  draining key waits for the drain, then opens fresh. Deleting a key refuses
  openers with `410` until the files are gone.

### 4. Migrations: on create, on open, or refused

Every fleet database gets the app's migrations and the shard framework sets
(version history, commit hooks, derivations), never the control-plane set.
A new database is always migrated. An existing one is migrated on open when
the boot auto-applies migrations, and refused with `503` otherwise, until
`AUTUMN_MIGRATE=1` migrates the whole fleet (`DatabaseFleet::migrate_all`,
bounded concurrency, per-database failures reported). Each database's run
holds `BEGIN IMMEDIATE`, so it is atomic and cross-process safe. The fleet is
mixed during a rollout: migrations must be expand-then-contract.

### 5. Replication: object storage is the medium between volumes

A host attaches one volume, so a volume cannot be the unit of durability or of
movement. Each fleet database ships its WAL to object storage under
`fleet/<name>/`, through the same engine and the same `[replication]`
destination as the control database:

- the fleet replicator ticks one `Replicator` per **open** database on one
  thread;
- a database it replicates runs with `wal_autocheckpoint = 0`, and only its
  replicator checkpoints. Fleet pools never inherit the process latch that
  disables auto-checkpointing for the control database, so a fleet that does
  not replicate keeps auto-checkpointing;
- closing a database waits for its in-flight connections, then ticks its
  replicator until nothing committed is unshipped, before the replicator's
  connection — the last one — goes. SQLite's last-connection checkpoint
  therefore never folds an unshipped frame into the file. When the
  destination is down, the replicator is parked rather than dropped: it keeps
  the file open, the loop keeps shipping until it catches up, health reports
  it under `closing`, and a reopen takes it back (one replicator per file);
- `DatabaseFleet::restore` rebuilds one database: into a private staging file
  first, then published under the guard above. With `restore_missing = true`,
  opening a database whose file is missing restores it first: a host that
  takes over a slot range serves it from the replicas. That restore also
  publishes with a no-replace link, so two processes restoring the same
  database at once end up on the first one's file.

Taking over a slot range needs the old owner stopped first (a fencing epoch is
roadmap item 2).

### 6. What stays on the control database

The outbox relay, derivation backfill, the job dashboard and shard-local jobs
enumerate configured shards at boot and do not visit fleet databases. The
commit-hook queue does: each open database runs its own commit-hook worker,
which stops when the database closes and resumes on the next open. It runs in
every process role, `web` included, because only a process that has the
database open can drain its queue; rows are claimed under `BEGIN IMMEDIATE`
with a lease, so two processes draining one database is safe.

## Consequences

### Positive

- One writer per tenant (or slot) instead of one per process.
- Per-tenant export (`backup`, `VACUUM INTO`), deletion (`delete` removes the
  file, its sidecars and the emptied bucket) and restore are file operations.
- Existing sharded app code runs unchanged; a Postgres-sharded app and a
  SQLite-fleet app share handlers and repositories.
- Bounded resources: `max_open × pool_size` connections, about
  `max_open × (2 × pool_size + 1)` file descriptors.
- Metrics stay bounded: a fleet database's metric and route label is
  `shard=fleet`; its name goes on spans and logs only.

### Negative

- Cross-tenant reads (`CrossShard`, `each_shard`, `across_tenants()`) open every
  database on disk. That is admin-path work, not a hot path.
- Outbox rows and derivation backfill are not supported on fleet databases
  yet (roadmap item 3). Enqueue through the control database.
- A replicated database pays one base snapshot per open/close cycle, because
  the replicator's generation state lives in memory. Size `max_open` so hot
  tenants stay open.
- Replicas are not deleted with their database (Litestream has the same
  gap). Lifecycle hooks can, and roadmap item 4 will.
- On Windows, deleting or restoring an existing database fails: SQLite opens
  files without delete sharing, so the guard connection blocks the unlink.
  Tracked in issue 3227.

## Roadmap

1. **CLI.** `autumn fleet list | migrate | backup | delete | restore`.
2. **Slot ownership and fencing.** A control-table epoch per slot range, so a
   host that takes over a range fences the old owner before
   `restore_missing` serves it (CloudKit's incarnation counter, Notion's
   logical-shard moves). This joins `cell_router.rs` (tenant → cell by the
   same slot hash) to the fleet.
3. **Fleet sweeper.** Visit databases with pending outbox rows, commit hooks
   or derivation backfill without waiting for traffic.
4. **Replica lifecycle.** Delete a database's replica with it; verify
   restores by sampling.
5. **Query the lake.** Cross-tenant analytics over the replicas in object
   storage (snapshots plus WAL segments), in the spirit of Athena / DuckDB
   over S3, instead of opening every file on a production volume.

## Alternatives considered

- **`[[database.shards]]` with SQLite URLs.** A fixed list cannot express one
  database per tenant, and 16384 slot entries are unmanageable.
- **`ATTACH` many databases to one connection.** Capped at 10 (125 at most),
  and still one writer per connection.
- **A process per tenant (Durable Objects style).** Hard isolation, but it
  needs an orchestrator; a fleet in one process keeps Autumn's deploy model.
- **libSQL / LiteFS / rqlite.** Different products with their own operations
  story. The SQLite tier stays a file on a local volume plus WAL shipping.
- **Directory routing table.** Routing a fleet by mode and template keeps the
  hot path free of a control-database lookup. Pinning a whale tenant to its
  own host belongs to the slot-ownership work (roadmap 2).

## Evidence

- `autumn/src/fleet_layout.rs` tests: id validation, templates, enumeration.
- `autumn/src/db/fleet.rs` tests: single-flight open, LRU and idle eviction,
  busy databases never evicted, delete and tombstone, misplaced-file refusal,
  `VACUUM INTO` backup, refusal of pending migrations, lifecycle hooks.
- `autumn/tests/sqlite_fleet.rs`: `ShardedDb`, a `#[repository(tenant_scoped,
  sharded)]` repository, `CrossShard` and `each_shard` against a tenant fleet
  and a slot fleet over HTTP; `400` / `404` / `409` paths; `db:fleet` health.
