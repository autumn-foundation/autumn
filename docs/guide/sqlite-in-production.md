# SQLite in production

Autumn supports **two production database tiers**, and you pick one per app:

- **SQLite** — an embedded, single-file database linked directly into your
  binary. A production deploy is *one process plus one data file*: no database
  server to install, secure, patch, or babysit. This is the zero-ops tier for a
  single host — a small VPS, an appliance, a [daemon](./daemon.md), or a
  self-hosted single-binary app via [`autumn deploy`](./deployment.md).
- **Postgres** — a networked server you run alongside the app. This is the
  scale-out tier: it unlocks [read replicas](./repositories.md),
  [native sharding](./sharding.md), multi-replica
  [scheduled tasks](./scheduled-multi-replica.md), and
  [Postgres full-text search](./full-text-search.md) — the features that only
  make sense when more than one process talks to the same data.

Both tiers run the **same battery** — models and repositories, embedded
migrations with the production-safety classifier, durable `#[job]` background
work, `#[scheduled]` tasks, DB-backed sessions and auth, and
[`autumn db backup`/`restore`](./daemon.md#database-backups). The difference is
that on SQLite the coordination primitives that Postgres implements with a
networked server collapse to their **single-host** form, and the genuinely
distributed features are **refused at boot** rather than silently degraded. This
guide is the published contract for exactly which is which.

> **Status.** The SQLite production tier lands in slices under issue #1614.
> Postgres remains the default for `autumn new`; SQLite is an opt-in target. The
> **Status** column in the matrix below reflects the rollout — a row marked
> *planned* names the slice that delivers it. The boot-refuse guarantees
> (the "fails fast" rows) are part of the contract from the first
> SQLite-enabled release, so an unsupported configuration never boots into a
> surprise at first query.

---

## When to choose SQLite vs Postgres

SQLite is an excellent production database when your workload fits inside its
two structural constraints. Neither is a bug to be worked around — they are the
shape of an embedded, single-file engine, and understanding them is how you
choose the right tier.

### The single-writer ceiling

SQLite serializes writes: at any instant **one writer** holds the database, and
concurrent write transactions queue behind it (Autumn runs SQLite in WAL mode,
so readers never block writers and multiple readers run concurrently — but
writes are still one-at-a-time). For the vast majority of apps — a solo
developer's SaaS, an internal tool, an appliance, a personal service — write
volume never approaches that ceiling and the operational simplicity is a pure
win.

Choose **Postgres** when you expect sustained, high-concurrency write load, many
independent writers contending on hot rows, or write throughput that a single
serialized writer cannot keep up with. That is the point of the scale-out tier.

### The single-host constraint

SQLite is a file on local disk. Every process that reads or writes it must run
on the **same host** with the file on **local** storage (not NFS/networked
filesystems, whose locking cannot be trusted). This is deliberate and per the
issue's scope: Autumn's SQLite tier is **single-host, single-writer only**.

That means the SQLite tier does not do:

- **Multiple app replicas** sharing one database over the network. If you need
  to run more than one process against the same data, that is Postgres.
- **Networked or multi-writer SQLite** (LiteFS, rqlite, libsql/Turso) — out of
  scope; those are different products, not a mode of this tier.
- **Streaming replication** (Litestream-style). Durability for the SQLite tier
  is the [snapshot backup](#backup-restore-scrub-retention) story, not
  continuous log shipping.

If your deployment is genuinely one host, all of the above are non-constraints
and SQLite gives you the whole framework with none of the server toil. If your
deployment is (or is about to become) many hosts, choose Postgres — and note
that because configuration is uniform across tiers, moving is a config change,
not a rewrite.

### Rule of thumb

| Your deployment is… | Choose |
| --- | --- |
| One host, one process, zero-ops priority | **SQLite** |
| One host, write volume comfortably below a single serialized writer | **SQLite** |
| Multiple replicas / multiple hosts sharing data | **Postgres** |
| Read replicas, sharding, or heavy write concurrency | **Postgres** |
| You need Postgres FTS, `LISTEN/NOTIFY`, or advisory-lock leader election | **Postgres** |

---

## Support matrix

Every framework capability falls into exactly one of three buckets on SQLite:

- ✅ **Works** — same behavior as Postgres (the mechanism may differ; the
  contract does not).
- ⚠️ **Degrades (documented)** — works on a single host, with a coordination
  primitive that collapses to its single-host form. The behavior is defined
  below, not a silent no-op.
- ⛔ **Fails fast** — refused at **boot** (or at generate time), with an
  actionable message, never at first query. This is a genuinely distributed
  feature that has no single-host meaning.

| Capability | Postgres | SQLite | Mechanism / behavior on SQLite | Status |
| --- | :---: | :---: | --- | --- |
| Core models / CRUD / repositories | ✅ | ✅ | Same repository API and query path. | #1614 core |
| Embedded migrations + `autumn migrate` up/down | ✅ | ✅ | Same embedded migrations run on SQLite. | #1614 core |
| `autumn migrate check` (production-safety classifier) | ✅ | ✅ | Same classifier; SQLite-specific rewrites classified. | #1614 core |
| Migration serialization (concurrent boot) | ✅ `pg_advisory_lock` | ⚠️ | Single-host `BEGIN IMMEDIATE` reservation instead of a cluster advisory lock — safe because only one host applies. | #1614 core |
| Sessions + auth (DB-backed) | ✅ | ✅ | Session/auth tables live in SQLite; no external store. | #1614 core |
| Durable `#[job]` background jobs | ✅ `FOR UPDATE SKIP LOCKED` | ✅ | Single-writer claim on the jobs table — durable and restart-safe, **no Redis required**. | #1614 core |
| `#[scheduled]` tasks | ✅ advisory-lock leader election | ⚠️ | Single host is always the leader; every tick fires locally (no election needed). | #1614 core |
| Distributed lock (`autumn_web::lock`) | ✅ `pg_advisory_lock` | ⚠️ / ⛔ | Single-host mutual exclusion within the process; a multi-replica configuration is refused at boot. | #1614 core |
| Feature-flag / experiment cache invalidation | ✅ `LISTEN/NOTIFY` | ⚠️ | In-process invalidation only (single host has nothing to notify). | #1614 core |
| `autumn db backup` / `restore` | ✅ `pg_dump`/`pg_restore` | ✅ | Online-safe snapshot of the data file (safe against a live app). | #1595 |
| `autumn db scrub` | ✅ | ✅ | Runs against the SQLite file. | #1602 |
| Retention sweeps | ✅ | ✅ | Runs against the SQLite file. | #1605 |
| `autumn deploy` data-file persistence | ✅ | ✅ | SQLite data file treated as **persistent state**; deploy/rollback never clobbers it. | #1607 |
| Read replicas (`replica_url`) | ✅ | ⛔ | **Boot-refuse.** No networked replicas on a single-file DB — out of scope. | contract |
| Sharding / shard directory | ✅ | ⛔ | **Boot-refuse.** Native sharding is Postgres-only. | contract |
| Postgres FTS scaffold (`--search`, `tsvector`) | ✅ | ⛔ | **Rejected at generate time.** `tsvector` has no SQLite equivalent; FTS5 is a later slice. | contract |
| Streaming replication (Litestream-style) | n/a | ⛔ | Out of scope; snapshot backup is the durability story. | contract |
| Multi-writer / networked SQLite (LiteFS, rqlite) | n/a | ⛔ | Out of scope; single-host, single-writer only. | contract |

---

## How the degrades behave

Each ⚠️ row above works on a single host. Here is the exact behavior, so you can
reason about it rather than guess.

### Migration serialization

On Postgres, concurrent booters race for a `pg_advisory_lock` so that exactly
one process applies pending migrations while the rest wait and then observe no
pending work (see [Migrations](./migrations.md)). On SQLite there is only one
host, so there is nothing to serialize *across* — but Autumn still guards the
apply with a **`BEGIN IMMEDIATE`** reservation so that two processes on the same
box (for example an old and new binary overlapping during a restart) cannot
interleave DDL. The safety property is identical; the primitive is local.

### `#[scheduled]` tasks

The [multi-replica scheduler](./scheduled-multi-replica.md) uses advisory-lock
leader election so that a fleet fires each tick exactly once. On SQLite the
single host **is always the leader** — there is no fleet to elect within — so
every scheduled tick fires locally with no coordination round-trip. Design
scheduled tasks to be idempotent regardless of tier; the at-most-once-per-tick
contract holds because there is only one ticker.

### Distributed lock

[`autumn_web::lock::Lock`](./distributed-locks.md) is a cluster-wide named lock
built on Postgres advisory locks. On SQLite it provides **single-host** mutual
exclusion (the whole point of the tier is that "the cluster" is one process).
Because a SQLite deployment is single-host by definition, a lock used for
across-host coordination has no counterpart — so a configuration that declares
multiple replicas against a SQLite database is **refused at boot**, not silently
downgraded to a no-op that would let two replicas both believe they hold it.

### Feature-flag / experiment cache invalidation

On Postgres, a flag or experiment change fans out to every replica via
`LISTEN/NOTIFY` so caches invalidate fleet-wide. On SQLite the invalidation is
**in-process only** — correct and immediate, because the single host is the only
cache there is. See [Feature flags](./feature-flags.md) and
[Experiments](./experiments.md).

### Durable jobs without Redis

This is the headline of the tier. `#[job]` work is durable and restart-safe on
SQLite with **no Redis and no Postgres** — the job queue is a table in the same
SQLite file, and a worker claims work with a single-writer claim (the SQLite
analogue of `FOR UPDATE SKIP LOCKED`). A crash mid-job leaves the row reclaimable
after restart, exactly as on Postgres. A job or scheduler *backend* that
genuinely requires Redis or Postgres is refused at boot rather than pretending to
be durable. See [Jobs](./jobs.md).

### Backup, restore, scrub, retention

`autumn db backup` takes an **online-safe snapshot** of the SQLite file — safe to
run against a live app, and it neither corrupts nor blocks it. `restore`,
[`db scrub`](./daemon.md) (#1602), and retention sweeps (#1605) all operate on
the SQLite file through the same command surface as Postgres. Snapshot backup —
not streaming replication — is the durability story for this tier.

---

## SQLite field-type support

The backend-aware generator maps model field kinds to SQLite storage types at
`autumn generate` time. Like the capability matrix above, this tier lands in
slices: a field kind is either **mapped** to a working SQLite column type, or
**rejected at generate time** with an actionable message that names its tracking
issue — never emitted as output that compiles on Postgres but breaks at migrate
time on SQLite.

| Field kind | On SQLite | SQLite type | Note |
| --- | :---: | --- | --- |
| `String` / `Text` | ✅ | `TEXT` | |
| `i32` | ✅ | `INTEGER` | |
| `i64` / references (foreign keys) | ✅ | `INTEGER` | Reference columns are `i64` foreign keys. |
| `bool` | ✅ | `INTEGER` | Stored as `0` / `1`. |
| `f32` | ✅ | `REAL` | |
| `f64` | ✅ | `REAL` | |
| `Bytea` | ✅ | `BLOB` | |
| `NaiveDateTime` | ✅ | `Timestamp` (TEXT) | Core, ungated `diesel::sql_types::Timestamp`. |
| `DateTime<Utc>` | ⛔ | — | **Rejected at generate time — #1924.** Its only working SQLite conversion needs diesel's `TimestamptzSqlite`, exported only behind diesel's `sqlite` feature, which the generated app's Postgres-oriented deps do not enable. |
| `Enum` | ⛔ | — | **Rejected at generate time — #1924.** The generated enum emits only Postgres (`Pg`) `ToSql`/`FromSql<Text>` impls, so SQLite repository loads/inserts do not compile. |
| `Uuid` | ⛔ | — | **Rejected at generate time — #1924.** No working diesel SQLite `FromSql`/`ToSql` in the app's diesel feature set. |
| `Decimal` | ⛔ | — | **Rejected at generate time — #1924.** Same reason. |
| `Attachment` / `Blob` | ⛔ | — | **Rejected at generate time — #1924.** Same reason. |

Two additional generator shapes are refused on SQLite:

- **`--id uuid` primary keys** are rejected at generate time — the SQLite primary
  key is `INTEGER PRIMARY KEY AUTOINCREMENT`, and a UUID primary key has no
  working conversion yet. Tracked in #1905.
- **`--search` / FTS scaffold** is rejected at generate time — Postgres FTS uses a
  `tsvector` generated column and a GIN index, which SQLite lacks. SQLite FTS5 is
  a later slice. Tracked in #1910.

### Migration mechanics on SQLite

A few SQLite-specific mechanics apply when the generator emits migrations:

- **`ADD COLUMN NOT NULL` requires a default.** SQLite cannot add a `NOT NULL`
  column to an existing table without a default value, so a re-add that lacks one
  is **rejected at generate time** — on both the `up` (add) and rollback (re-add)
  paths — rather than emitting SQL that fails at migrate time.
- **Rollback drops indexes before columns.** On the SQLite rollback path the
  generator emits `DROP INDEX` before `DROP COLUMN`, since SQLite will not drop a
  column that an index still references.
- **Known limitation — dropping a pre-existing indexed column.** A
  `Remove…From…` migration that drops a column which was indexed by an *earlier*
  migration can still fail on SQLite, because the generator has no knowledge of
  the original table's indexes and so cannot emit the matching `DROP INDEX`
  first. Drop the index in the same migration, or drop the column via a manual
  table rebuild. Tracked under the SQLite migrations issue #1906.

---

## What is NOT supported on SQLite

These are Postgres-only by design. They are not missing features to be filed as
bugs; they are the scale-out tier's reason to exist, and every one of them
**fails fast at boot** (or, for the FTS scaffold, at generate time) with an
actionable message:

- **Read replicas** (`replica_url` / replica routing) — a single file has no
  networked replica to route reads to.
- **Native sharding** (the shard directory / multi-shard repositories) — see
  [Sharding](./sharding.md).
- **Streaming / continuous replication** (Litestream-style log shipping) — use
  snapshot [backups](#backup-restore-scrub-retention) for durability instead.
- **Multi-writer clustering / networked SQLite** (LiteFS, rqlite,
  libsql/Turso) — the tier is single-host, single-writer only.
- **Postgres full-text search** (the `--search` scaffold, `tsvector` columns,
  GIN indexes) — rejected at generate time because `tsvector` has no SQLite
  column type. SQLite FTS5 is a later slice, not this one; see
  [Full-text search](./full-text-search.md).

---

## Fail fast at boot, never at first query

The core promise of the two-tier design is that a **mismatched configuration is
caught the instant the app starts**, with a message that tells you what to fix —
never as a runtime surprise on some unlucky code path days later.

- A **Postgres-shaped setting on a SQLite app** (or a SQLite target where a
  Postgres feature is configured — a `replica_url`, a shard directory, a
  Postgres-only job/scheduler backend, a multi-replica lock) fails at boot with
  an actionable diagnostic.
- A **generator** that would emit output which compiles on Postgres but breaks
  on SQLite (for example the `tsvector` FTS scaffold) is rejected at **generate
  time**, with the reason stated — never silent output that fails later.

So the operational rule is simple: if a SQLite app boots, every feature it is
configured to use is supported on SQLite. There is no third state where an
unsupported feature lurks until first use.

---

## See also

- [Daemon mode: `autumn serve`](./daemon.md) — the single-binary local service
  shape, database backups, and where state lives.
- [Deployment](./deployment.md) and `autumn deploy` — persistent-state contract
  for the SQLite data file.
- [Migrations](./migrations.md) — the classifier, checksums, and advisory-lock
  serialization this guide contrasts against.
- [Jobs](./jobs.md) and [Multi-replica scheduled tasks](./scheduled-multi-replica.md).
- [Sharding](./sharding.md), [Repositories](./repositories.md), and
  [Full-text search](./full-text-search.md) — the Postgres-only scale-out
  features.
