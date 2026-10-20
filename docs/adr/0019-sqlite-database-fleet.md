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
  Delete and restore unlink the sidecars first and the main file last: once
  the main file is gone, another process can publish a new database at the
  path, and nothing of ours may touch it after that. A database created there
  after a delete is the documented outcome (a fresh database, when created on
  demand).
- A path lock covers every step that publishes or removes a database file:
  create, restore (explicit and on open) and delete. It is a lock file in
  `<root>/.fleet-locks/`, one of 256 chosen by a stable hash of the key. The
  lock is `flock` on Unix and `LockFileEx` on Windows, so the OS drops it when
  a process dies. A lock file is never deleted. A process that finds the path
  empty under the lock knows that none can publish there until it lets go, so
  it clears the stray sidecars of the absent database first: a stale `-wal`
  would replay onto the new file. Other keys on the same lock file wait, so
  hold it only for the publish. A restore on open holds it for the download
  too. The lock needs a file system with working `flock` (not NFS).
- Delete and restore install the drain first, so no new entry can appear.
  They then take the entry that was open and wait for its opener task to end.
  A running open is never interrupted, and a late opener cannot start one:
  the drain refuses it. The delete then drains the database that open made.
- `close_all` waits for owned delete, restore and provision tasks, then sets
  `shutting_down`. Every open then fails with `503`. It closes every database
  and waits until none is open or draining.
- Keys are checked against the fleet (mode, slot range, a tenant's own slot)
  before anything touches the disk.
- Closing calls `Pool::close` (a stale handle can no longer check out), waits
  for in-flight connections, then runs lifecycle hooks. An opener for a
  draining key waits for the drain, then opens fresh. Deleting a key refuses
  openers with `410` until the files are gone.
- Fleet operations are cancellation-safe. `open` (first open), `close`,
  `delete`, `restore` and `provision` run their work in owned tasks. A caller
  that is dropped (timeout, client disconnect) only stops waiting:
  - A claimed key always leaves its drain, so no later opener gets a stranded
    `410`, and `close_all` does not hang.
  - A first open that already started its `on_open` hooks ends in the open
    map. A later `close` or `delete` runs the matching `on_close`.
  - A `provision` can publish its file and then lose its caller. The fleet
    keeps a record of that creation. The next `provision` of the key reports
    it. Exactly one call does. `delete` clears the record. The record is in
    memory only.
  - `close_all` waits for opens that are still running, then closes them.

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
- at shutdown, the fleet keeps ticking parked replicators until all caught up
  or the final-flush deadline passes (`server.shutdown_timeout_secs`). For
  each database still unshipped, it logs an `error!` and lists the name under
  `unshipped` in the `replication:fleet` health details;
- `DatabaseFleet::restore` rebuilds one database: into a private staging file
  first, then published under the guard above. With `restore_missing = true`,
  opening a database whose file is missing restores it first: a host that
  takes over a slot range serves it from the replicas. That restore also
  publishes with a no-replace link, so two processes restoring the same
  database at once end up on the first one's file;
- processes that share a root elect one replicator per database. The owner
  holds an OS lock on the sidecar `<db>.replicator.lock`. Other processes open
  the database, never checkpoint it, and retry the lock on each tick. The OS
  frees the lock when the owner exits or dies, and the new owner starts a new
  generation. Two replicators would have independent generations, and a
  restore of the newest one could miss commits of the other. A lock file was
  chosen over a lease row in the database: a lease row writes to the WAL on
  every renewal, and it is copied into every replica;
- health reports the databases this process has open but does not own under
  `unowned`.

Section 7 fences the old owner when another host takes over a slot range.

### 6. Background work: the fleet sweeper

A fleet database runs background work only while it is open. The fleet
sweeper (`autumn_web::db::fleet_sweeper`, issue 3234) does the work of
databases that nobody asks for:

- **Index.** `_autumn_fleet_pending(key, kind, since)` on the control
  database lists the databases with work. `kind` is `outbox`, `hooks` or
  `backfill`. The sweeper never opens a file to find work. Two more columns
  hold the backoff of a database (`visit_after`, `attempts`).
- **Mark.** An outbox write and a commit-hook enqueue write a row for their
  database after the insert and before the commit, while they hold the
  tenant write lock. Enqueue code has only a connection: it finds its fleet
  by the file of the connection (`pragma_database_list`) and its key by the
  identity row. When the row cannot be written, the write fails.
- **Clear.** A visit removes a row only under the tenant write lock
  (`BEGIN IMMEDIATE`), after it finds no work of that kind. Marks and clears
  therefore never interleave, and committed work always has a row. The lock
  order is always tenant database, then control database. A rolled-back
  write can leave a row with no work; the next visit removes it.
- **Open.** When a database opens, it gets a row for each kind of work it
  holds. This covers work that an earlier process left without a row. If
  the row cannot be written, the open fails.
- **Derivations.** `_autumn_fleet_derivation_epochs` on the control database
  records each derivation fingerprint a worker has seen, in order. A worker
  with a new fingerprint records it, then lists the files and marks each
  database `backfill`. It opens none to do this. Only the binary with the
  newest fingerprint runs a backfill (checked before each database) or
  removes a `backfill` row (checked in the same `DELETE`). When a newer
  fingerprint lands during a backfill, the old binary marks the database
  again, so the new one reconciles it. A fingerprint seen before is never recorded again, so an
  old binary that restarts in a rolling deploy cannot undo the new
  definitions. A rollback to older derivations needs a `resweep`. When a
  database opens, the current binary runs up to 8 backfill batches before
  the first request; the sweeper does the rest.
- **Visit.** A worker process (its `role` runs workers) opens each listed
  database with `DatabaseFleet::open_background`. That open never creates a
  file, never restores one from a replica, and never takes the fleet past
  `max_open`: with no room, the database waits for the next pass. It does
  not count as a use, so the database is the first LRU victim. The visit
  drains hooks, sends the outbox (in a tenant fleet, handlers run in the
  tenant's scope), purges old outbox rows and runs the backfill. Then
  `close_if_unused` closes the database, but only when nobody used it and
  nobody holds a lease on it. A deleted database loses its rows at its next
  visit.
- **Roles.** Every role installs the index and the marks; a failed install
  stops the process. A `web` process visits only the databases it has open
  with hook rows: it drains the hooks and clears the rows. A worker leaves
  the hook rows of a closed database alone for `hook_grace` (5 s), as the
  process that has it open drains them.
- **Bounds.** Each pass visits at most 64 databases, 4 at a time, with 256
  messages and 4 backfill rounds each. When a visit leaves work and does
  nothing, the database waits 1 s, then twice as long each time, up to
  5 min. New work of the same kind ends the wait. A visit that made
  progress and left work moves the database behind the others that are
  due, so busy databases take turns. A pass reads the due rows of the open
  databases and the oldest due rows, and puts them in one order: the row
  due first goes first, then the oldest row. An open database goes first
  only on a tie, as its visit opens nothing. A new row is due at the time of
  its mark. Thus busy open databases never hold back a closed one
  (issue 3350). A worker runs its first pass when it
  starts, before it waits for the interval. The index stores this wait, so a stuck database never hides the
  others. `/actuator/health` reports `fleet:sweeper`: the backlog
  (databases), the oldest pending age, and the counts of the last pass.

The sweeper opens and closes databases through the normal fleet paths, so
the rules of #3231 (workers stop before the pool closes) and #3230 (one
replicator per database across processes) hold for its opens too.

### 7. Slot ownership: epochs and fencing (issue #3233)

`restore_missing` lets a host with an empty volume take over a range. Without
fencing, the old owner can still write and ship, and the two copies diverge.
`[database.fleet.ownership]` adds a per-range ownership epoch. The open path
and the replication path both check it.

- **Table.** `_autumn_fleet_slot_owners(range_start, range_end, owner_id,
  epoch, lease_expires_at)` on the control database, and a one-row epoch
  counter. Ranges use the slot hash of `sharding.rs` and `cell_router.rs`.
  Rows never overlap.
- **Decisions.** `OwnershipTable` is a pure state machine. A store loads the
  rows, applies one decision and writes the result in one `BEGIN IMMEDIATE`
  transaction. So when two processes race a claim, one wins.
  - Claim: the range is free or each lease on it ended, or it is the
    claimer's own row with the same bounds. A live row of another shape
    refuses the claim, also for its own owner: an old process of that owner
    can still serve the rest of the row. A new epoch is above the counter
    and above the newest epoch a replicator found (`claim_above`). The own
    row with the same bounds keeps its epoch (a restart, or a second process
    on the volume).
  - Renew: same owner and epoch, else the range is lost.
  - Release: the lease ends now. The epoch stays. Only the last process on
    the volume releases (a shared OS lock on the root tells), and only when
    nothing is unshipped.
- **Open path.** `DatabaseFleet::open`, `provision` and `open_background`
  check the slot before anything touches the disk. If the host does not own
  it: `FleetError::NotOwner`, `421`. So `restore_missing` runs only for an
  owned key. Each `FleetDatabase` carries its epoch. After an open, the fleet
  reads the epoch again; a database of another epoch is fenced, not served.
- **Stale files.** Each file records the newest epoch it was opened under
  (`_autumn_fleet_epoch`). A host can get a range back after another host
  held it. Its files on disk are then older than the replica. So an open under
  a newer epoch than the file's lists `<root>/epochs/`. If the replica holds a
  newer epoch than the file, the file is restored from the replica first.
- **Lose.** A lost range closes its open databases without a worker settle.
  The close still waits for connections in use. The sweeper and the fan-outs
  skip slots this host does not own.
- **Local lease.** A holder serves until it sent its last renew plus
  `ttl - ttl/3`, on its monotonic clock. That is before the lease in the
  table ends. The ownership loop closes lapsed ranges before it renews, and
  while a renew hangs. Clock skew plus the longest request must be less than
  `ttl/3`.
- **Replication.** A replicator ships under its database's epoch. It writes
  `<root>/epochs/<epoch>` and, before the snapshot upload,
  `generations/<id>/epoch-<epoch>`. A restore and the retention sweep order
  generations by (epoch, time). The sweep never removes a generation without
  `snapshot.json` that is newer than the floor (an upload in progress). A
  replicator with no epoch records the newest epoch at the destination, so
  turning ownership off does not order new generations below old ones. The
  replicator lists `<root>/epochs/` at each new generation and every 60 s. A
  newer epoch stops it (`ReplicationError::Fenced`), and the host loses the
  range. A replica with no markers is epoch 0, so old replicas restore as
  before.
- **One replicator per database (#3230).** The epoch is the election token
  across hosts: only the owner of the current epoch opens the database. An
  old replicator can still ship for up to 60 s, into its old epoch's
  generation; the restore order makes that harmless. The lock file still
  elects one process inside the host. A parked replicator that reopens under
  a new epoch starts a new generation.

This mirrors the `FencingToken` of `lock.rs` (#3053), CloudKit's incarnation
counter and Notion's logical-shard moves. The model and its proofs are in
`verification/fleet_slot_ownership.rs`.

All hosts must use one ownership table. The built-in store is the control
database, a local SQLite file, so it fences the processes of one host only.
Hosts on different machines need a `SlotOwnershipStore` on a store they all
reach.

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
- Each outbox write and commit-hook enqueue on a fleet database adds three
  reads on the tenant database and one on the control database, and one
  control write when the database has no row yet.
- Do not enqueue on a fleet database while you hold a write transaction on
  the control database. The mark needs the control write lock, so it waits
  for `busy_timeout` and then fails.
- An outbox row written outside a transaction commits before its mark. If
  the mark then fails, the call returns an error for a committed row: do not
  retry it. The next open of the database writes the row.
- The first pass of a worker after a derivation change marks every database.
  The sweeper then opens each once.
- A replicated database pays one base snapshot per open/close cycle, because
  the replicator's generation state lives in memory. Size `max_open` so hot
  tenants stay open.
- Replicas are not deleted with their database (Litestream has the same
  gap). Lifecycle hooks can, and roadmap item 4 will.
- On Windows, deleting or restoring an existing database fails: SQLite opens
  files without delete sharing, so the guard connection blocks the unlink.
  Tracked in issue 3227.

## Roadmap

1. **CLI.** `autumn fleet list | migrate | backup | delete | restore`
   ([#3232](https://github.com/autumn-foundation/autumn/issues/3232)). See
   [Operating the fleet](../guide/sqlite-fleet.md#operating-the-fleet).
2. **Slot ownership and fencing.** Done: see section 7
   ([#3233](https://github.com/autumn-foundation/autumn/issues/3233)).
3. **Fleet sweeper.** Done: see section 6
   ([#3234](https://github.com/autumn-foundation/autumn/issues/3234)).
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
- `autumn-cli/tests/integration/fleet_cli.rs`: each `autumn fleet` subcommand
  against a temporary fleet root; `restore` from a local replica.
- `autumn/src/db/fleet_sweeper.rs` tests: index marks and clears, marks from
  a connection, two fleets in one process.
- `autumn/tests/sqlite_fleet_sweeper.rs`: an outbox row relayed after its
  database closes, hooks drained without a request, a derivation added in a
  migration backfilled in a closed tenant, `max_open` and leases respected,
  the `fleet:sweeper` health indicator.
- `autumn/src/db/fleet_ownership.rs` tests: the state machine (claim, renew,
  lapse, release, re-shape) and a proptest of its invariants; two hosts
  racing a claim on one store; the open path's `421`; an old owner closed
  after it lost the epoch; a fenced replicator; fan-out of owned slots only.
- `autumn/src/replication/restore.rs` tests: a restore takes the newest
  epoch's generation before the newest time.
- `autumn/tests/sim_fleet_ownership.rs`: a partitioned old owner in virtual
  time stops serving before the new owner claims.
- `autumn/tests/sqlite_fleet.rs`: `ShardedDb`, a `#[repository(tenant_scoped,
  sharded)]` repository, `CrossShard` and `each_shard` against a tenant fleet
  and a slot fleet over HTTP; `400` / `404` / `409` paths; `db:fleet` health.
