# ADR 0032: Query the Fleet Lake

- Status: Accepted
- Date: 2026-11-02
- Deciders: Autumn maintainers
- Tags: db, sqlite, fleet, replication, analytics

## Context

ADR 0019 roadmap item 5 (issue
[#3236](https://github.com/autumn-foundation/autumn/issues/3236)).

`CrossShard`, `each_shard` and `across_tenants()` open each database on the
volume. This is correct for admin paths. It is not correct for analytics:

- It uses `max_open` slots that live tenants need, and it closes hot
  databases.
- It runs only on the host that has the volume.
- `across_tenants()` refuses a grouped `sum` or `avg` across shards.

With `[replication]` on, each tenant database is already in object storage
under `<base>/fleet/tenant/<id>/`. That is a data lake keyed by tenant.

The bundled SQLite (3.53) has `SQLITE_MAX_ATTACHED = 10`. The issue text
says 125, which is the compile-time maximum, not the value in this build.

## Options

1. **SQLite `ATTACH` in batches**, with a `UNION ALL` view per table.
2. **DuckDB** with its `sqlite` scanner over the restored files.
3. **Parquet export** per table, partitioned by tenant and date.
4. **SQLite materialisation**: restore each tenant, `ATTACH` it alone, copy
   its rows into one scratch lake file, then query that file.

## Decision

Option 4.

- It adds no dependency. DuckDB is a large C++ build. Cargo unifies
  features, so each `sqlite` app would build it. Option 4 also needs no new
  license or `cargo deny` review.
- One `ATTACH` at a time removes the limit of 10. Option 1 must keep every
  tenant of a batch attached, and a query cannot cross batches.
- The lake is one SQLite file. `GROUP BY`, `JOIN`, window functions and CTEs
  work across all tenants. This includes the grouped `sum` and `avg` that
  `across_tenants()` refuses.
- `LakeSnapshot::persist` writes the lake with `VACUUM INTO`. DuckDB,
  `sqlite3` and other tools can read that file. This is the export path
  until a user needs Parquet.

Parquet (option 3) is deferred. DuckDB (option 2) is rejected for now.

### How it works

1. **List.** One `list` of `<base>/fleet/` finds each tenant, its
   generations and its tombstone marker. The lake never lists the volume.
2. **Decide.** The lake skips a tenant when a tombstone row exists, or when
   the marker covers its newest generation. It uses
   `fleet_tombstones::restore_allowed`, the same rule as a restore. A marker
   with no generation is a delete or a purge in progress.
3. **Plan.** `restore::plan_from_listing` plans from the same listing. The
   restore applies this plan, so the decision and the restore see one
   generation. The lake checks `restore_allowed` again with the planned
   generation. When the listing is older than the replica (a head past the
   listed segments, or a listed marker that is gone), the lake lists that
   tenant again and decides once more.
4. **Restore.** `workers` threads restore one chunk of tenants into a
   private scratch directory (mode `0700`, named from `Entropy`).
5. **Ingest.** In tenant order, the lake attaches each file
   (`mode=ro&immutable=1`), checks its identity row, and copies each app
   table in one transaction. The journal is in memory, so a failed tenant
   rolls back. It adds the column `_autumn_tenant`. Lake columns have no
   declared type, so each value keeps its storage class. They also have no
   affinity and the `BINARY` collation: a `COLLATE NOCASE` comparison or a
   text-to-number conversion of the source does not apply. Columns union by
   name: a column that a tenant does not have is `NULL`.
6. **Query.** A `mode=ro` connection makes a temporary view of the SQL and
   sets `query_only`. The SQL must be one `SELECT`, `WITH` or `VALUES`
   statement. A subquery that SQLite cannot flatten computes each value
   once, so each value has one type. Rows stream through `quote()`, which
   keeps the storage class and round-trips a real. Text goes as hex, because `quote()` stops at a
   NUL.

`_autumn_lake_tenants` records each loaded tenant: its generation and its
effective time.

### Consistency

- Each tenant is consistent at its own snapshot: one generation, replayed to
  one segment.
- There is **no atomic snapshot across tenants**. Each tenant lags by its own
  replication delay.
- `as_of(t)` takes each tenant at its newest state at or before `t`. It does
  not make one common instant. A tenant made after `t` is skipped as
  `AbsentAtTarget`.
- The tombstone decision is made now, not at `t`. A tenant deleted after `t`
  is not in a query at `t`.

### Safe defaults and opt-ins

- A lake with no tombstone source (`with_tombstones`) refuses to load.
  `markers_only()` is the explicit opt-in. Between the row and the marker of a delete, a
  markers-only lake still loads the tenant.
- A tenant that fails (restore, marker, identity, ingest) fails the load.
  `skip_failed_tenants()` is the explicit opt-in to report it and continue.
- `DatabaseFleet::lake()` refuses a scratch directory or a kept copy under
  the fleet root. It compares canonical paths, so a symlink does not get
  past it.
- The SQL is operator SQL. Do not pass request input to it.

### Limits

- Slot fleets are out of scope. A slot database holds many tenants. The lake
  refuses a destination that holds slot databases only.
- Virtual tables (FTS) and views are not copied. Their shadow tables and the
  framework tables (`_autumn_*`, `__diesel_*`) are not copied.
- Each load restores every tenant. Disk use is about the size of the loaded
  tables, plus one restored file per worker. The default scratch is the
  system temporary directory, which can be memory on some hosts.
- The lake runs app SQL, so its statements have no fixed fingerprints. The
  Ledger gate does not see it: the Docker sweep has no `sqlite` build.

## Consequences

- Analytics run on any host with read access to object storage and,
  optionally, the control database.
- `autumn fleet lake <sql> [--out] [--keep] [--timestamp]` runs one query
  from the command line and writes CSV.
- `restore::plan` now reads segment keys from its first listing. This is one
  `list` call less per restore.
- Future options: an incremental lake cache keyed by generation, a
  map/reduce mode that runs SQL per tenant, and a native Parquet writer.

## Evidence

- `autumn/src/db/fleet_lake/tests.rs`: aggregate equals `fleet.each` with no
  volume; row, marker and purge exclusion; the row-without-marker window;
  point in time; schema union; refused writes; scratch removal; one listing.
- `autumn/tests/sqlite_fleet.rs`
  `the_lake_matches_across_tenants_with_no_volume`: the lake equals
  `CrossShard` / `across_tenants()` count, rows and per-tenant counts.
- `autumn-cli/tests/integration/fleet_cli.rs` `fleet_lake_*`: the CLI.
- `verification/fleet_lake.rs`: the inclusion rule, the aggregate, and that
  the lake writes nothing under the fleet root.
