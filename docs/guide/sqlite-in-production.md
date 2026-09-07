# SQLite in production

Autumn supports **two production database tiers**, and you pick one per app:

- **SQLite** — an embedded, single-file database linked directly into your
  binary. A production deploy is *one process plus one data file*: no database
  server to install, secure, patch, or babysit. This is the zero-ops tier for a
  single host — a small VPS, an appliance, a [daemon](./daemon.md), or a
  self-hosted single-binary app via [`autumn deploy`](./deployment.md).
- **Postgres** — a networked server you run alongside the app. This is the
  scale-out tier: it unlocks [read replicas](./repositories.md),
  [native sharding](./sharding.md), and multi-replica
  [scheduled tasks](./scheduled-multi-replica.md) — the features that only
  make sense when more than one process talks to the same data.
  ([Full-text search](./full-text-search.md) is **not** in this list — it now
  works on both backends, see below.)

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
- **Streaming replication to another *server*.** Continuous replication ships
  the write-ahead log to offsite *storage* for recovery
  ([durability](#durability-continuous-replication-and-point-in-time-restore),
  #1628) — it does not feed a second live process. There is still exactly one
  writer and exactly one host.

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
| You need Postgres-specific FTS features (language-stemming dictionaries), `LISTEN/NOTIFY`, or **cross-host** leader election | **Postgres** |

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
| Embedded migrations + `autumn migrate` up/down | ✅ | ✅ | **Startup** migrations apply on **file-backed** SQLite through diesel's `MigrationHarness` (unlocked); an **in-memory** target with registered migrations is refused at boot (the migrated schema is lost before the runtime pool anchors it). The `autumn migrate` CLI up/down still routes through the Postgres advisory-lock path (`hold_migration_lock` → `PgConnection`), so it is not available for a `sqlite://` URL yet. (The separate **declarative** `autumn schema migrate` verb — see [Declarative schema](./declarative-schema.md) — *does* apply pending migrations on SQLite, unlocked, **but only when the CLI was built with the non-default `sqlite` cargo feature** — the default/published `autumn` binary is Postgres-only and stops with a "rebuild with `--features sqlite`" error.) | ⚠️ **Partial** — startup migrations apply on file-backed SQLite (MigrationHarness; in-memory + migrations boot-refused); the classic `autumn migrate` CLI up/down is Postgres-only (planned) |
| `autumn migrate check` (production-safety classifier) | ✅ | ✅ | Offline SQL-file safety linter (reads no DB URL, so it does not fail on a `sqlite://` target); its safety rules target Postgres migration semantics — there is no SQLite-specific classification yet. | ⚠️ **Partial** — the linter runs (no DB connection), but its rules are Postgres-oriented; no SQLite-specific classification |
| Migration serialization (concurrent boot) | ✅ `pg_advisory_lock` | ⚠️ | Startup migrations run **unlocked** — no advisory lock and no `BEGIN IMMEDIATE` reservation on the migration path. Concurrent same-host starts are not serialized by an explicit reservation; they rely on SQLite's single-writer semantics plus the pool `busy_timeout`. (Note: application **write-RMW** sites *do* issue `BEGIN IMMEDIATE` since #1996 — this row is only about the migration path.) | ⚠️ **Not serialized** — no advisory lock / no migration-path `BEGIN IMMEDIATE`; explicit reservation is a known gap (planned) |
| Sessions + auth (DB-backed) | ✅ | ✅ | Session/auth tables live in SQLite; no external store. The `generate auth` tracked-sessions store binds `RuntimeBackend` rather than `diesel::pg::Pg`, so it compiles and runs on either backend, and its migration DDL and scaffolded guide are emitted in the app's own dialect (#1908 / #1927). Its `schema.rs` block is backend-independent (every column kind the table uses maps to the same diesel sql-type on both backends), not dialect-forked. The cookie-session backends (`[session] backend = "memory" | "redis"`) are backend-independent and unchanged. **Still Postgres-only, and out of this row's scope:** the framework `DbApiTokenStore` (`api_tokens` — machine tokens, not login sessions) is typed `Pool<AsyncPgConnection>` with Postgres-only DDL, and the `--starter saas` scaffold pins `AsyncPgConnection` throughout. | ✅ **Available now** (#1908, behind the `sqlite` feature) |
| Durable `#[job]` background jobs | ✅ `FOR UPDATE SKIP LOCKED` | ✅ | `jobs.backend = "sqlite"`: a single-writer claim on the `autumn_jobs` table in the app's own file — durable and restart-safe, **no Redis required**. Retries, backoff, dead-lettering, uniqueness windows, concurrency limits, and the job dashboard match Postgres. | ✅ **Available now** (#1907) |
| `#[scheduled]` tasks | ✅ advisory-lock leader election | ⚠️ | `scheduler.backend = "in_process"` (the default) fires every tick locally, because one process is always the leader. `scheduler.backend = "sqlite"` leases each tick in a table, so several processes on the host elect exactly one leader per tick. | ✅ **Available now** (#1907) |
| Distributed lock (`autumn_web::lock`) | ✅ `pg_advisory_lock` | ⚠️ | `autumn_web::lock::Lock` takes a lease row in `autumn_locks` instead of a session lock, so the processes on the host contend. A live holder renews; a dead one's lock frees at the lease expiry. | ⚠️ **Available now** (#1907) — single-host scope, lease not session, not re-entrant |
| Feature-flag / experiment cache invalidation | ✅ `LISTEN/NOTIFY` | ⚠️ | In-process invalidation only (single host has nothing to notify). | ⛔ **Planned — #1905** |
| `autumn db backup` / `restore` | ✅ `pg_dump`/`pg_restore` | ✅ | Online-safe snapshot of the data file (safe against a live app). Backup tooling is still `pg_dump`/`pg_restore`-shaped today. | ⛔ **Planned — #1909** |
| `autumn db scrub` | ✅ | ✅ | Runs against the SQLite file. | ⛔ **Planned — #1909** |
| Retention sweeps | ✅ | ✅ | Runs against the SQLite file. | ⛔ **Planned — #1909** |
| `autumn deploy` data-file persistence | ✅ | ✅ | SQLite data file treated as **persistent state**; deploy/rollback never clobbers it. | ⛔ **Planned — #1909** |
| Read replicas (`replica_url`) | ✅ | ⛔ | **Boot-refuse.** No networked replicas on a single-file DB — out of scope. | ✅ **Available now — boot-refuse** |
| Sharding / shard directory | ✅ | ⛔ | **Boot-refuse.** Native sharding is Postgres-only. | ✅ **Available now — boot-refuse** |
| Full-text search (`--searchable` / `#[searchable]`) | ✅ `tsvector` + GIN | ✅ FTS5 | **Available now on both backends.** Postgres uses a `tsvector` generated column + GIN index; SQLite uses an external-content **FTS5** virtual table with `unicode61` tokenization and `bm25` ranking (weights from `#[searchable(weight=…)]`). The `--searchable` / `#[searchable]` scaffold generates on both (#1910 / #2047). | ✅ **Available now** |
| Continuous replication + point-in-time restore | n/a | ✅ | Built into the running process (#1628): the write-ahead log is shipped to an S3-compatible or filesystem destination as it is written, and `autumn db replica restore` rebuilds the database on a fresh box, optionally at a chosen instant. See [durability](#durability-continuous-replication-and-point-in-time-restore). | ✅ **Available now** |
| Streaming replication to a second live *node* | n/a | ⛔ | Out of scope; continuous replication targets storage, not a standby process. | Contract (out of scope) |
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
  - `ADD COLUMN NOT NULL` without a default (on both the add and rollback re-add
    paths).
  - `DROP INDEX` emitted before `DROP COLUMN` on the forward **and** rollback
    paths (a plain `--index` is dropped before its column is removed).
- **Backend-aware `generate auth` / `generate mailer` (#1927 / #1908)** — both now
  scaffold SQLite-dialect migrations on a SQLite app (`INTEGER PRIMARY KEY
  AUTOINCREMENT`, `DEFAULT CURRENT_TIMESTAMP`, `INTEGER` foreign keys) instead of
  being refused, and the generated auth session store is typed against
  `::autumn_web::RuntimeConnection` so it compiles on either backend.
- **DB-backed sessions store on SQLite (#1908)** — the `generate auth`
  tracked-sessions store bounds its query functions by
  `::autumn_web::RuntimeBackend` instead of a hard-coded `diesel::pg::Pg`, so the
  scaffolded store compiles and runs against the SQLite `RuntimeConnection`. The
  scaffolded `docs/guide/session-management.md` now hands the operator SQL in the
  app's own dialect (`datetime('now', '-90 days')`, SQLite retrofit DDL) instead
  of Postgres-only `NOW() - INTERVAL` / `BIGSERIAL`.
- **`autumn doctor` SQLite awareness** — a SQLite app is no longer nagged about a
  missing `pg_dump` or a non-`postgres://` URL.

**Not in this slice — scaffold smoke tests on SQLite.** A scaffolded app still
carries the **Postgres-shaped** (`#[ignore]`d) smoke test. A SQLite-native
scaffold smoke harness needs the SQLite `TestDb` (a testcontainer) that lands
with the runtime slice — until then there is no SQLite backend to run
SQLite-dialect smoke SQL against, so the generated smoke test remains
Postgres-shaped. Tracked under the runtime slice #1905.

The support-matrix rows still marked **Planned** name follow-on subsystem slices
whose SQLite support has not landed yet
(backup/restore/scrub/retention/deploy persistence #1909). A **Planned** row
does **not** mean the app refuses to boot — the runtime
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
leader election so that a fleet fires each tick exactly once. SQLite has no
advisory locks, so it gets two single-host coordinators instead:

- `scheduler.backend = "in_process"` (the **default**). The single process is
  always the leader, so every tick fires locally with no coordination
  round-trip. This is right for the ordinary one-process deployment.
- `scheduler.backend = "sqlite"`. Each `(task, tick)` is leased in the
  `autumn_scheduler_leases` table in the app's own database file, so **several
  processes on the one host** elect exactly one leader per tick. Use it when a
  web tier and a worker tier run side by side, or across a rolling restart where
  the old and new process overlap.

The lease carries an expiry, not a session. The row is what makes the tick
claimed, and it stays for the whole of `scheduler.lease_ttl_secs` (default 300)
whether the leader finished or died — so a second process whose timer reaches the
same tick a moment later cannot run it again. The next acquire reaps the row once
it expires.

Set the TTL longer than both the spread between the processes' timers and the
longest a tick body can take, so a live leader is never preempted mid-tick.

This is stricter than the Postgres coordinator, whose `pg_advisory_unlock` frees
the tick key the moment the leader finishes.

`scheduler.backend = "postgres"` is refused at boot under SQLite, with a message
naming both substitutes.

Design scheduled tasks to be idempotent regardless of tier.

### Distributed lock

[`autumn_web::lock::Lock`](./distributed-locks.md) is a cluster-wide named lock
built on Postgres advisory locks. On SQLite the same API takes a lease row in an
`autumn_locks` table in the app's own file, so it provides **single-host** mutual
exclusion across the processes sharing that file. Three differences a caller can
observe:

- **The scope is one host.** Processes sharing the database file contend; two
  hosts do not.
- **It is a lease, not a session.** A holder that dies frees the lock at the
  lease expiry rather than wedging it, and a live holder renews in the
  background, so a long critical section is not preempted. Postgres releases on
  connection loss instead.
- **It is not re-entrant.** A Postgres session lock can be taken twice on one
  connection; a second `try_lock` on the same name in the same process observes
  `None`.

`lock()` polls rather than waiting server-side, because SQLite has no
`pg_advisory_lock` to block in. Tune the interval with `with_poll_interval`.
Because a SQLite deployment is single-host by definition, a lock used for
across-host coordination has no counterpart. Every Postgres-only primitive that
would imply one — a `replica_url`, a shard directory, a Postgres job or
scheduler backend — is **refused at boot**, not silently downgraded to a no-op
that would let two replicas both believe they hold it.

### Feature-flag / experiment cache invalidation

On Postgres, a flag or experiment change fans out to every replica via
`LISTEN/NOTIFY` so caches invalidate fleet-wide. On SQLite the invalidation is
**in-process only** — correct and immediate, because the single host is the only
cache there is. See [Feature flags](./feature-flags.md) and
[Experiments](./experiments.md).

### Durable jobs without Redis

This is the headline of the tier. `#[job]` work is durable and restart-safe on
SQLite with **no Redis and no Postgres**. Set:

```toml
[jobs]
backend = "sqlite"
```

The queue is the `autumn_jobs` table in the same SQLite file. A worker claims
work with a single-writer claim — one `UPDATE … WHERE id = (SELECT … LIMIT 1)
RETURNING …`, which is the SQLite analogue of `FOR UPDATE SKIP LOCKED`, because
SQLite serializes writers. A crash mid-job leaves the row reclaimable: a claim
older than `jobs.sqlite.visibility_timeout_ms` (default 30s) is re-enqueued, or
dead-lettered when its attempts are spent. The runtime creates the table and its
indexes at start, so no migration is needed.

Everything the Postgres queue gives you carries over: attempt counting,
exponential backoff, dead-lettering, `#[job(unique)]` windows,
`#[job(concurrency = N)]` limits, named queues and `[jobs] pin`, the actuator
backlog gauges, and the `/admin/jobs` dashboard — which reads the table, so
every process on the host sees the same queue. `enqueue_tracked` records go in
the same file too, so `GET /_autumn/jobs/{token}` survives a restart and works
across a web/worker split.

Three differences from Postgres, all by design:

- **Workers poll.** SQLite has no `LISTEN`/`NOTIFY`. An enqueue in the same
  process wakes a worker directly; work another process enqueued is seen within
  `jobs.sqlite.poll_interval_ms` (default 250ms). Lower it for latency, raise it
  to cut idle wakeups.
- **The queue is host-local, and must be a file.** Two processes on one host
  share it; two hosts do not. A split web/worker role on an **in-memory** target
  is refused at boot, because each process would get its own database. Nothing
  enforces the two-hosts case at boot — the tier's Postgres-only primitives
  (`replica_url`, shards, `jobs.backend = "postgres"`,
  `scheduler.backend = "postgres"`) are each refused, but a second host pointed
  at the same file over a network filesystem is not detected. Do not do it; see
  [The single-host constraint](#the-single-host-constraint).
- **History is pruned by the runtime, not by `autumn db retention`.** The sweep
  behind `autumn db retention` is Postgres-only (#1909). Instead the SQLite job
  runtime prunes its own tables: expired tracked-job records always, and
  terminal `autumn_jobs` rows when `retention.job_history` is set. Leave that
  window unset and job history is kept forever, exactly as on Postgres.

`jobs.backend = "local"` (the default) stays the right choice when the work does
not need to survive a restart: it is in-process and needs no table.
`jobs.backend = "postgres"` is refused at boot under SQLite, with a message
naming the durable substitute. See [Jobs](./jobs.md).

### Backup, restore, scrub, retention

`autumn db backup` takes an **online-safe snapshot** of the SQLite file — safe to
run against a live app, and it neither corrupts nor blocks it. `restore`,
[`db scrub`](./daemon.md) (#1602), and retention sweeps (#1605) all operate on
the SQLite file through the same command surface as Postgres. Snapshots are the
coarse-grained, cross-backend story; for second-granularity durability see
[Durability: continuous replication](#durability-continuous-replication-and-point-in-time-restore)
below, which composes with them rather than replacing them.

---

## Durability: continuous replication and point-in-time restore

Snapshot backups bound your data loss at **one backup interval** — hours, if you
back up nightly. Continuous replication (#1628) bounds it at **seconds**, from
inside the process you already run. No sidecar to install, supervise and monitor;
no second binary; no new credential conventions.

```toml
[replication]
enabled = true

[replication.s3]
bucket = "myapp-replicas"
region = "auto"
endpoint = "https://<account-id>.r2.cloudflarestorage.com"
access_key_id_env = "AUTUMN_REPLICA_ACCESS_KEY_ID"
secret_access_key_env = "AUTUMN_REPLICA_SECRET_ACCESS_KEY"
force_path_style = true
```

Credentials are supplied by **env-var indirection**: config names the variables,
never the secrets, exactly as `[backup.offsite]` does (#1619). Every key also has
an `AUTUMN_REPLICATION__*` override, so an all-env deployment needs no TOML at
all. A `path = "/mnt/backup-disk/replica"` destination replicates to a directory
instead — a second disk, an NFS/SSHFS mount, or a bind-mounted volume.

By default replication refuses to share a bucket with the app's own blob storage
(`[storage.s3]`): a lifecycle rule written for user uploads would quietly expire
your replicas. Set `allow_shared_bucket = true` to opt in.

### The RPO contract

| Setting | Default | What it bounds |
| --- | --- | --- |
| `rpo_secs` | `10` | Steady-state data loss if the machine dies right now. |

> **One exception to the objective.** Opening a new generation compresses and
> uploads a fresh base snapshot on the replication thread, and a commit made
> *during* that upload is not offsite until it finishes — on a large database,
> longer than `rpo_secs`. Everything committed before the rollover is shipped
> first, so only that window is affected. `snapshot_interval_secs` (default one
> hour) sets how often it happens.

| `sync_interval_secs` | `rpo_secs / 2` | How often committed frames are shipped. Must not exceed `rpo_secs`. |
| `snapshot_interval_secs` | `3600` | How long one generation runs before a fresh base snapshot. |
| `max_wal_bytes` | `16777216` | WAL size that forces a checkpoint (and the next WAL index). |
| `retention_hours` | `168` | How far back a point-in-time restore can reach. |
| `verify_interval_secs` | `21600` | How often the replica is proved restorable. |

**The contract: at most `rpo_secs` of committed writes are lost when the machine
is destroyed**, under normal write load. Only *committed transactions* are ever
shipped — a segment always ends on a commit boundary, so a replica is never half
a transaction.

That bound covers the machine *disappearing*. A **planned** stop — a deploy, a
restart, a `SIGTERM` — loses nothing: once in-flight requests have drained, the
replicator ships one final time and shutdown waits for that flush inside the same
`server.shutdown_timeout_secs` budget as the other shutdown phases. If the budget
runs out first the process still exits, and says so in the log.

Two surfaces report it, from two vantage points. `autumn db replica status`
(add `--json` for monitoring) reads the **destination** and reports the current
generation, how many segments it holds, the instant a restore would land on, and
the lag to now — so it works from any machine with the credentials, including one
where the app is not running. `/actuator/health` reports what the **running
process** knows, under the `sqlite-replication` indicator: `lag_seconds`,
`generation`, `segments_shipped`, `pending_bytes`, `last_verified_at`.

**What you actually pay for.** Steady-state upload is the size of your WAL
writes, not the size of your database. The `-wal` file cannot grow forever, so
the replicator checkpoints it once `max_wal_bytes` is reached — but a checkpoint
only opens the next *WAL index* inside the current generation, which costs
nothing extra offsite. A full base snapshot is uploaded once per
`snapshot_interval_secs` (hourly by default), and that interval is also what
bounds how much WAL a restore has to replay. Lowering
`snapshot_interval_secs` makes restores faster and uploads larger; raising it
does the reverse.

### Alerting

The `sqlite-replication` indicator goes `DOWN` when lag exceeds three times the
configured RPO, when shipping is failing, or when a periodic verification could
not restore the replica. An indicator that stays unhealthy past the alerter's
grace period is escalated on every channel configured under
[alerting](./operator-alerts.md) (#1610), with a recovery notice when it clears — no
separate replication alert to configure.

**Verification is a real restore**, not a checksum: on `verify_interval_secs` the
process downloads the replica into a scratch directory, replays it, and runs
`PRAGMA integrity_check`. "Uploaded" is never mistaken for "restorable".

### Fresh-box recovery runbook

The disk is gone. On a new machine, with nothing but the `autumn` binary, your
`autumn.toml`, and the destination credentials in the environment:

```bash
export AUTUMN_ENV=prod
export AUTUMN_REPLICA_ACCESS_KEY_ID=…
export AUTUMN_REPLICA_SECRET_ACCESS_KEY=…

# 1. What does the destination hold, and how fresh is it?
autumn db replica status

# 2. Rebuild the database (writes to the configured database.url).
#    --force clears the production-profile guard; the box is empty, so no
#    --overwrite is needed.
autumn db replica restore --force

# 3. Start the app.
autumn serve
```

To land **before** a specific moment — a bad migration, a runaway delete — add a
timestamp inside the retention window:

```bash
autumn db replica restore --timestamp 2026-09-02T14:29:00Z --force --overwrite
```

`--force` and `--overwrite` are deliberately separate: `--force` clears the
production-profile guard, `--overwrite` allows replacing a database file that is
already there. A recovery drill that always passes `--force` therefore cannot
silently destroy a live database.

The command prints the instant it actually landed on. A timestamp older than the
window is **refused**, with the oldest reachable instant named, rather than
silently rounded to whatever happened to be there.

Restore is gated the same way `autumn db restore` is (#1595): the production
profile needs `--force`, and overwriting an existing database file needs `--force`
whatever the profile. Nothing is written until the rebuilt database has passed
`PRAGMA integrity_check`, so a refused restore never leaves a half-built database
in place of a good one.

### What makes it safe against a live app

In WAL journal mode SQLite writes the main database file **only during a
checkpoint**. When replication is enabled Autumn's pool therefore sets
`PRAGMA wal_autocheckpoint = 0` and the replicator becomes the only component
that ever checkpoints — which is also why the tier is single-host and
single-writer (see [the single-host constraint](#the-single-host-constraint)).
Two consequences worth knowing:

- **A dead destination costs disk, never data.** A checkpoint is attempted only
  once everything in the WAL is already offsite, so an unreachable endpoint
  stalls checkpointing and the `-wal` file grows. Lag climbs, the health
  indicator goes `DOWN`, and you get paged. Watch free space on the database's
  filesystem alongside the lag.
- **Verification needs headroom.** A periodic verification restores the whole
  replica into a scratch directory beside the database, so it transiently needs
  about as much free space again on that filesystem. Lower
  `verify_interval_secs` to `0` if you would rather verify out of band with
  `autumn db replica verify` on another box.
- **Do not run a second writer.** A `sqlite3` shell that checkpoints — or a
  second app process on the same file — breaks the invariant. The
  single-host/single-writer contract is not advisory here.

Replication does not block or slow the writer: it reads the `-wal` with ordinary
file I/O, takes no database locks to ship, and its checkpoints use a one-second
busy timeout so a contended checkpoint gives up and retries on the next tick
rather than holding up a write.

### Limits of this slice

- SQLite only. Postgres deployments have mature continuous-archiving ecosystems
  (WAL-G, pgBackRest); `[replication]` is refused at boot on a Postgres target.
- One destination. Multi-destination fan-out and cross-region policy are out of
  scope, the same boundary as #1619.
- Server-side encryption at rest and TLS in transit; client-side encryption of
  replicated data is a named follow-up.
- Replication is not a read replica and not clustering — see
  [What is NOT supported on SQLite](#what-is-not-supported-on-sqlite).

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
> **`generate auth` / `generate mailer` now generate on SQLite (#1927 / #1908).**
> Historically refused at generate time, both now scaffold SQLite-dialect
> migrations on a SQLite app (`INTEGER PRIMARY KEY AUTOINCREMENT`, `DEFAULT
> CURRENT_TIMESTAMP`, `INTEGER` foreign keys — including the `--totp`,
> `--magic-link`, `--oauth`, and `--passkeys` tables and the `generate mailer
> --list-unsubscribe` suppression table). The generated auth **DB-backed session
> store** is typed against `::autumn_web::RuntimeConnection` (which resolves to the
> Postgres connection by default and the SQLite connection under the `sqlite`
> feature), so it compiles on whichever backend the app selected. Its query
> functions bind `::autumn_web::RuntimeBackend` for the same reason (#1908), and
> the scaffolded session-management guide emits its SQL in the app's dialect.

> **Full-text search now generates on SQLite (#2047).** The `--searchable` /
> `#[searchable]` scaffold — historically rejected at generate time on SQLite —
> now emits a backend-appropriate index on both backends: a `tsvector` generated
> column + GIN index on Postgres, and an external-content **FTS5** virtual table
> (`unicode61` tokenization, `bm25` ranking) on SQLite. See
> [Full-text search](./full-text-search.md#6-sqlite-fts5).

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
**fails fast at boot** with an actionable message:

- **Read replicas** (`replica_url` / replica routing) — a single file has no
  networked replica to route reads to.
- **Native sharding** (the shard directory / multi-shard repositories) — see
  [Sharding](./sharding.md).
- **Replication to a second live node** (a standby process serving from the same
  data). Continuous replication to offsite *storage* is supported — see
  [durability](#durability-continuous-replication-and-point-in-time-restore) —
  but there is no second process to fail over to.
- **Multi-writer clustering / networked SQLite** (LiteFS, rqlite,
  libsql/Turso) — the tier is single-host, single-writer only.
- **A server-side statement timeout** has no SQLite equivalent — diesel's async
  `SqliteConnection` exposes no interrupt hook — so a non-zero
  `database.statement_timeout` on a `sqlite` URL is now **refused at boot**
  (`PoolError::UnsupportedBackend`, #2034) rather than silently ignored;
  long-running statements are otherwise bounded by `busy_timeout` (lock
  contention) only, not a wall-clock cap.

> **Now supported on SQLite (previously listed here).** Full-text search
> (the `--searchable` / `#[searchable]` scaffold) is available via **FTS5** (#2047),
> searchable repositories generate and run on SQLite, and **version-history**
> (`versioned = true`) columns are supported on the SQLite runtime with JSON
> stored as `TEXT` (#2034). A single-record write-RMW site also issues an
> explicit **`BEGIN IMMEDIATE`** write reservation on SQLite (via
> `scoped_immediate_transaction`, through diesel's `AnsiTransactionManager`, so
> nested transactions become savepoints) — #2034 / #2038. The one remaining
> `BEGIN IMMEDIATE` gap is the **startup migration path**, which still applies
> unlocked with no explicit reservation (planned).

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
  on SQLite (for example a `Uuid` / `Decimal` field kind or an `--id uuid`
  primary key) is rejected at **generate time**, with the reason stated — never
  silent output that fails later.

So the operational rule is simple: if a SQLite app boots, every feature it is
configured to use is supported on SQLite. There is no third state where an
unsupported feature lurks until first use.

---

## See also

- [Daemon mode: `autumn serve`](./daemon.md) — the single-binary local service
  shape, database backups, and where state lives.
- [Alerting](./operator-alerts.md) — the channels a replication-lag or
  verification-failure alert is escalated on.
- [Deployment](./deployment.md) and `autumn deploy` — persistent-state contract
  for the SQLite data file.
- [Migrations](./migrations.md) — the classifier, checksums, and advisory-lock
  serialization this guide contrasts against.
- [Jobs](./jobs.md) and [Multi-replica scheduled tasks](./scheduled-multi-replica.md).
- [Sharding](./sharding.md) and [Repositories](./repositories.md) — the
  Postgres-only scale-out features.
- [Full-text search](./full-text-search.md) — available now on **both**
  backends (Postgres `tsvector`/`GIN`, SQLite FTS5).
