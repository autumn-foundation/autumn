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
> *planned* names the slice that delivers it and is **not available in this
> build**. The boot-refuse guarantees (the "fails fast" rows) are part of the
> contract from the first SQLite-enabled release, so an unsupported
> configuration never boots into a surprise at first query.
>
> **The SQLite runtime has landed** (#1614). A `sqlite://` app now boots, runs
> its startup migrations, and serves, with a working connection pool and
> repository CRUD. The **Status** column below marks each capability
> **Available** when it is verified in this build, or **Planned — #NNNN** when
> that subsystem's SQLite support is still landing in a follow-on slice — a
> *Planned* row no longer means the app refuses to boot, only that the named
> subsystem is not yet wired for SQLite. This guide is
> the published support contract for the rollout; rows are marked by the slice
> that delivers them. What ships *today* is listed under
> [What ships in this slice](#what-ships-in-this-slice).

---

> **Update (this release):** the SQLite *runtime* has now landed behind the `sqlite` cargo feature. The sections further down that describe SQLite being *refused at boot* or list runtime rows as *planned* predate that work; the runtime now boots, migrates, and serves against a `sqlite://` database as described immediately below.

## Runtime (behind the `sqlite` feature)

Enable the runtime by building the application with the `sqlite` cargo feature (it must be enabled only by the end application, never by a library). Autumn then boots and serves against a `sqlite://` URL:

- **Connection type.** A `RuntimeConnection` alias abstracts the backend: `diesel_async::AsyncPgConnection` by default, and a `SyncConnectionWrapper<SqliteConnection>` under the `sqlite` feature. Generated repositories and hand-written queries take `&mut RuntimeConnection`, so they compile against either backend.
- **Pool pragmas.** Each pooled connection is set up with `PRAGMA busy_timeout = 5000` (first, so later statements queue on it), `PRAGMA journal_mode = WAL`, `PRAGMA synchronous = NORMAL`, and `PRAGMA foreign_keys = ON`. A read-only SQLite target skips the two writing pragmas (WAL + `synchronous`). An in-memory database is pinned to a single pooled connection.
- **Migrations.** Startup migrations run through diesel's `MigrationHarness` on a plain `SqliteConnection` with **no advisory lock** (SQLite is single-writer, so there is nothing to coordinate). Only `busy_timeout` is set on the migration connection — `foreign_keys = ON` is deliberately *omitted* there because it breaks table-recreating migrations. This applies to **file-backed** SQLite only: an **in-memory** target (`sqlite::memory:` / `:memory:` / `file::memory:`, including `cache=shared`) with registered startup migrations is **refused at boot** (`std::process::exit(1)`), because the schema is applied on a transient migration connection and is lost before the runtime pool anchors it. An in-memory target with *no* registered migrations is unaffected (it is the default test-harness configuration).
- **Repository CRUD.** Generated `#[repository]` / `#[model]` CRUD targets SQLite via two seams: `maybe_for_update!` expands to a plain read on SQLite (which has no `SELECT … FOR UPDATE`), so a pessimistic-lock read degrades to a plain read while write-write correctness still rests on the optimistic `lock_version` check plus the pool `busy_timeout`; and `backend_select! { pg => {…}, sqlite => {…} }` picks backend-specific SQL for the shapes that differ (multi-row batch insert vs. per-row loop, batched `ON CONFLICT` upsert vs. per-row upsert, and `RETURNING` handling). Tenant scoping and `lock_version` semantics are preserved on both backends.

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

The **SQLite** glyph below is the *eventual* single-host contract for each
capability; the **Status** column tells you which slice delivers it and
therefore whether it is available **today**. A row whose Status reads
**Planned** names a subsystem whose SQLite support is still landing in a
follow-on slice — the app still boots and serves, that subsystem just is not
wired for SQLite yet. Every capability falls into one of three eventual
buckets on SQLite:

- ✅ **Works** — same behavior as Postgres (the mechanism may differ; the
  contract does not).
- ⚠️ **Degrades (documented)** — works on a single host, with a coordination
  primitive that collapses to its single-host form. The behavior is defined
  below, not a silent no-op.
- ⛔ **Fails fast** — refused at **boot** (or at generate time), with an
  actionable message, never at first query. This is a genuinely distributed
  feature that has no single-host meaning.

| Capability | Postgres | SQLite (eventual) | Mechanism / behavior on SQLite | Status (today) |
| --- | :---: | :---: | --- | --- |
| Core models / CRUD / repositories | ✅ | ✅ | Same repository API and query path on the SQLite runtime pool. | ✅ **Available now** (behind the `sqlite` feature) |
| Embedded migrations + `autumn migrate` up/down | ✅ | ✅ | **Startup** migrations apply on **file-backed** SQLite through diesel's `MigrationHarness` (unlocked); an **in-memory** target with registered migrations is refused at boot (the migrated schema is lost before the runtime pool anchors it). The `autumn migrate` CLI up/down still routes through the Postgres advisory-lock path (`hold_migration_lock` → `PgConnection`), so it is not available for a `sqlite://` URL yet. | ⚠️ **Partial** — startup migrations apply on file-backed SQLite (MigrationHarness; in-memory + migrations boot-refused); `autumn migrate` CLI up/down is Postgres-only (planned) |
| `autumn migrate check` (production-safety classifier) | ✅ | ✅ | Offline SQL-file safety linter (reads no DB URL, so it does not fail on a `sqlite://` target); its safety rules target Postgres migration semantics — there is no SQLite-specific classification yet. | ⚠️ **Partial** — the linter runs (no DB connection), but its rules are Postgres-oriented; no SQLite-specific classification |
| Migration serialization (concurrent boot) | ✅ `pg_advisory_lock` | ⚠️ | Startup migrations run **unlocked** — no advisory lock and no `BEGIN IMMEDIATE` reservation. Concurrent same-host starts are not serialized by an explicit reservation; they rely on SQLite's single-writer semantics plus the pool `busy_timeout`. | ⚠️ **Not serialized** — no advisory lock / no `BEGIN IMMEDIATE`; explicit reservation is a known gap (planned) |
| Sessions + auth (DB-backed) | ✅ | ✅ | Session/auth tables live in SQLite; no external store. | ⛔ **Planned — #1908** |
| Durable `#[job]` background jobs | ✅ `FOR UPDATE SKIP LOCKED` | ✅ | Single-writer claim on the jobs table — durable and restart-safe, **no Redis required**. | ⛔ **Planned — #1907** |
| `#[scheduled]` tasks | ✅ advisory-lock leader election | ⚠️ | Single host is always the leader; every tick fires locally (no election needed). | ⛔ **Planned — #1907** |
| Distributed lock (`autumn_web::lock`) | ✅ `pg_advisory_lock` | ⚠️ / ⛔ | Single-host mutual exclusion within the process; a multi-replica configuration is refused at boot. | ⛔ **Planned — #1905** (multi-replica boot-refuse ships now) |
| Feature-flag / experiment cache invalidation | ✅ `LISTEN/NOTIFY` | ⚠️ | In-process invalidation only (single host has nothing to notify). | ⛔ **Planned — #1905** |
| `autumn db backup` / `restore` | ✅ `pg_dump`/`pg_restore` | ✅ | Online-safe snapshot of the data file (safe against a live app). Backup tooling is still `pg_dump`/`pg_restore`-shaped today. | ⛔ **Planned — #1909** |
| `autumn db scrub` | ✅ | ✅ | Runs against the SQLite file. | ⛔ **Planned — #1909** |
| Retention sweeps | ✅ | ✅ | Runs against the SQLite file. | ⛔ **Planned — #1909** |
| `autumn deploy` data-file persistence | ✅ | ✅ | SQLite data file treated as **persistent state**; deploy/rollback never clobbers it. | ⛔ **Planned — #1909** |
| Read replicas (`replica_url`) | ✅ | ⛔ | **Boot-refuse.** No networked replicas on a single-file DB — out of scope. | ✅ **Available now — boot-refuse** |
| Sharding / shard directory | ✅ | ⛔ | **Boot-refuse.** Native sharding is Postgres-only. | ✅ **Available now — boot-refuse** |
| Postgres FTS scaffold (`--search`, `tsvector`) | ✅ | ⛔ | **Rejected at generate time.** `tsvector` has no SQLite equivalent; FTS5 is a later slice (#1910). | ✅ **Available now — reject at generate** |
| Streaming replication (Litestream-style) | n/a | ⛔ | Out of scope; snapshot backup is the durability story. | Contract (out of scope) |
| Multi-writer / networked SQLite (LiteFS, rqlite) | n/a | ⛔ | Out of scope; single-host, single-writer only. | Contract (out of scope) |

---

## What ships in this slice

The SQLite runtime has landed (#1614): a `sqlite://` app **boots, runs its
startup migrations, serves against a working connection pool, and runs
repository CRUD**, on top of the earlier **config detection, boot-time
validation, backend-aware generator, `autumn doctor` awareness, and this
published support contract**. Available **today**:

- **`sqlite:` / `file:` config recognition + boot-time validation** — a SQLite
  target is recognized and validated when the URL carries one of the accepted
  schemes: `sqlite:///var/lib/app.db` (canonical `sqlite://` followed by an
  absolute path), `sqlite:app.db` (the shorter scheme-only form),
  `sqlite::memory:` (in-memory), or `file:app.db`. A **bare filesystem path**
  such as `/var/lib/app.db` is intentionally **not** recognized and fails
  validation — prefix it with `sqlite://` (or `sqlite:` / `file:`). An
  **in-memory** target is recognized for a no-migration configuration, but
  combining it with **registered startup migrations is refused at boot** — the
  migrated schema lives only on the transient migration connection and is gone
  before the runtime pool anchors it, so a durable deploy must be
  **file-backed**. Postgres-only
  settings (read replicas, shard directory, Postgres-only job/scheduler
  backends, multi-replica locks) are **refused at boot** with an actionable
  message rather than silently at first query.
- **Backend-aware DDL generator** — `autumn generate` emits SQLite column types
  for the supported field kinds (see
  [field-type support](#sqlite-field-type-support)).
- **Generate-time rejections**, each naming its tracking issue:
  - `Uuid` / `Decimal` / `Attachment` / `DateTime<Utc>` / `Enum` field kinds —
    #1924.
  - `--id uuid` primary keys — #1905.
  - `--search` / FTS scaffold — #1910.
  - `ADD COLUMN NOT NULL` without a default (on both the add and rollback re-add
    paths).
  - `DROP INDEX` emitted before `DROP COLUMN` on the forward **and** rollback
    paths (a plain `--index` is dropped before its column is removed).
  - `generate auth` / `generate mailer` on a SQLite app — #1927 (a **generate-time**
    refusal only; `generate destroy`/revert of an existing scaffold is
    unaffected).
- **`autumn doctor` SQLite awareness** — a SQLite app is no longer nagged about a
  missing `pg_dump` or a non-`postgres://` URL.

**Not in this slice — scaffold smoke tests on SQLite.** A scaffolded app still
carries the **Postgres-shaped** (`#[ignore]`d) smoke test. A SQLite-native
scaffold smoke harness needs the SQLite `TestDb` (a testcontainer) that lands
with the runtime slice — until then there is no SQLite backend to run
SQLite-dialect smoke SQL against, so the generated smoke test remains
Postgres-shaped. Tracked under the runtime slice #1905.

The support-matrix rows still marked **Planned** name follow-on subsystem slices
whose SQLite support has not landed yet (sessions/auth #1908, durable jobs and
`#[scheduled]` tasks #1907, backup/restore/scrub/retention/deploy persistence
#1909). A **Planned** row does **not** mean the app refuses to boot — the runtime
boots and serves; those subsystems are simply not wired for SQLite until their
tracking issue lands.

---

## How the degrades behave

> These describe the single-host behavior on the SQLite runtime. The runtime now
> boots and serves, so the behaviors for landed capabilities apply today; those
> tied to a still-**Planned** subsystem take effect when that subsystem's SQLite
> slice lands.

Each ⚠️ row above works on a single host. Here is the exact behavior, so you can
reason about it rather than guess.

### Migration serialization

On Postgres, concurrent booters race for a `pg_advisory_lock` so that exactly
one process applies pending migrations while the rest wait and then observe no
pending work (see [Migrations](./migrations.md)). On SQLite there is only one
host, so there is nothing to serialize *across*. The startup path applies
migrations through diesel's `MigrationHarness` **unlocked** — there is no
advisory lock and, today, **no `BEGIN IMMEDIATE`** reservation. Two processes on
the same box overlapping during a restart (an old and new binary) are therefore
not serialized by an explicit reservation; they rely on SQLite's single-writer
semantics plus the pool `busy_timeout`. An explicit `BEGIN IMMEDIATE`
reservation to close that same-host overlap window is **planned, not yet
implemented**.

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

Additional generator shapes are refused on SQLite:

- **`--id uuid` primary keys** are rejected at generate time — the SQLite primary
  key is `INTEGER PRIMARY KEY AUTOINCREMENT`, and a UUID primary key has no
  working conversion yet. Tracked in #1905.
- **`--search` / FTS scaffold** is rejected at generate time — Postgres FTS uses a
  `tsvector` generated column and a GIN index, which SQLite lacks. SQLite FTS5 is
  a later slice. Tracked in #1910.
- **`generate auth` / `generate mailer`** are rejected on a SQLite app at
  generate time — their scaffolds emit Postgres-shaped models and store code with
  no working SQLite mapping yet, so they are refused before any files are written
  rather than emitted as output that breaks on SQLite. This is a **generate-time**
  refusal only — `generate destroy`/revert of an existing scaffold still works.
  Tracked in #1927.

> **Scaffold smoke tests are still Postgres-shaped.** The generated-scaffold smoke
> test (including the duplicate-`unique` rejection) uses
> `autumn_web::test::TestDb`, a **Postgres-only** testcontainer, and
> `TRUNCATE … RESTART IDENTITY`, and runs only under `cargo test -- --ignored`
> (it is `#[ignore]`d). There is no SQLite `TestDb` yet — it lands with the
> runtime slice (#1905) — so a scaffolded SQLite app still carries the
> Postgres-shaped smoke test rather than a SQLite-native one. A backend-aware
> scaffold smoke harness is deferred to the runtime slice #1905.

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
- **Scoped / searchable repositories** (`--search` / full-text-search repos) are Postgres-only and are not generated on the SQLite backend.
- **Version-history (`jsonb`) columns** rely on Postgres `jsonb` semantics and are not supported on the SQLite runtime.
- **A server-side statement timeout** has no SQLite equivalent; long-running statements are bounded by `busy_timeout` (lock contention) only, not a wall-clock cap.
- **Explicit `BEGIN IMMEDIATE` write reservation** is not issued anywhere in the migration path today — migration serialization is not implemented via one (it is planned) — and general application transactions use SQLite's default deferred behaviour.

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
