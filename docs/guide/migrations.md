# Migrations

Autumn embeds [Diesel](https://diesel.rs/) migrations in the compiled binary and
runs them at startup (dev) or via `autumn migrate run` (production). This guide
covers the advisory-lock serialisation that prevents schema divergence during
rolling deploys, how to monitor contention, and what to expect on non-Postgres
backends.

---

## Advisory lock serialisation

When several replicas boot at the same time or when `autumn migrate run` is
called from multiple deployment steps concurrently, every instance would
naively race to apply the same pending migrations. Diesel wraps each migration
in a transaction, so two processes applying the same migration can deadlock,
fail mid-DDL, or leave the schema half-applied.

Autumn prevents this by acquiring a **PostgreSQL session-level advisory lock**
before reading the pending-migration list. Only one process holds the lock at a
time; the rest wait (polling every 500 ms). Once the winner finishes, waiters
re-read the migration table, find no pending work, and exit successfully.

The lock covers:

* The embedded application migrations run by `AppBuilder::migrations(…)`.
* The Autumn framework migrations run by `autumn migrate run`.

---

## Lock key

The advisory lock uses a single `bigint` key:

```
MIGRATION_ADVISORY_LOCK_KEY = 0x6175_746E_5F6D_6967  (7 021 124 476 890 851 687)
```

The value is the big-endian encoding of the ASCII bytes `autn_mig`. It is
**stable across framework versions** so you can add permanent alerting rules
without consulting the source code.

PostgreSQL splits a `pg_advisory_lock(bigint)` key into two 32-bit halves
stored in `pg_locks`:

| Column     | Value        | Derivation            |
|------------|--------------|-----------------------|
| `classid`  | 1 635 087 470 | upper 32 bits of key |
| `objid`    | 1 601 005 927 | lower 32 bits of key |
| `objsubid` | 1            | session-level lock    |

---

## Monitoring contention

```sql
-- Active migration lock holders and waiters
SELECT
    pid,
    granted,
    mode,
    (SELECT query FROM pg_stat_activity WHERE pid = l.pid) AS query
FROM pg_locks l
WHERE locktype = 'advisory'
  AND classid = 1635087470
  AND objid   = 1601005927
  AND objsubid = 1;
```

A row with `granted = false` means a waiter is queued behind the current lock
holder. If rows remain indefinitely after a deploy, check whether a migration
process crashed mid-run; PostgreSQL will release the lock when the connection
closes.

---

## Wait timeout

The default wait is **60 seconds**. If the lock is not acquired within that
window the process fails with:

```
migration advisory lock not acquired within 60s;
another process may still be running migrations
```

The timeout can be overridden per call when using the Rust API:

```rust
use autumn_web::migrate::{run_pending_locked, DEFAULT_LOCK_WAIT_TIMEOUT};
use std::time::Duration;

// Use the default (60 s)
run_pending_locked(database_url, MIGRATIONS, None)?;

// Override to 120 s
run_pending_locked(database_url, MIGRATIONS, Some(Duration::from_secs(120)))?;
```

For the `autumn migrate run` CLI the timeout is always the default.

---

## Wrapping an external migration process

If you invoke an external migration tool (e.g. a raw `diesel` subprocess) and
want it covered by the same advisory lock, use `hold_migration_lock`:

```rust
use autumn_web::migrate::{hold_migration_lock, DEFAULT_LOCK_WAIT_TIMEOUT};

let _guard = hold_migration_lock(database_url, DEFAULT_LOCK_WAIT_TIMEOUT)?;
// Lock is held for the lifetime of `_guard`.
// Run external process here …
// Lock is released when `_guard` drops.
```

This is exactly what `autumn migrate run` does internally before shelling out
to the `diesel` CLI.

---

## Non-Postgres backends

Advisory locks are a **PostgreSQL-specific** primitive. SQLite and in-memory
test harnesses do not support them.

* **SQLite / in-memory** — These backends are single-process by nature and do
  not need cross-process serialisation. Call `run_pending` directly; no lock is
  acquired or needed.
* **Tests using `TestDb`** — `TestDb` starts a real Postgres container, so the
  advisory lock is acquired normally.

If you write tests that call `run_pending_locked` against a non-Postgres
database the connection will fail before the lock query is issued, and the
function returns `MigrationError::Connection`.

---

## Content checksums — never edit an applied migration

Autumn records a SHA-256 of every migration's `up.sql` in a framework-owned
table, `autumn_migration_checksums`, the first time that migration is applied.
Before every subsequent apply (both `autumn migrate` and startup auto-migrate)
each applied migration's on-disk `up.sql` is re-hashed and compared against the
recorded value. A mismatch means the migration was **edited after being
applied** — the deployed schema silently forks from what a fresh build would
produce — so the run refuses to continue with a fail-fast error:

```
migration 20260101000000 checksum mismatch: recorded <hex-a> but on-disk
content hashes to <hex-b>. Migrations must never be edited after being
applied — add a new migration instead, or run the documented re-baseline
command if this change was deliberate.
```

The rule is simple: **never edit an applied migration.** Add a new migration
that expresses the change instead — that keeps every environment reproducible
from the same source tree.

The same guard also catches a migration that was **deleted or renamed after
being applied**: if a version has a recorded checksum but its `up.sql` is gone
from the source tree, a fresh database would no longer run it, so `autumn
migrate` refuses to continue (`missing` in `status`). The remedy is the same —
never delete or rename an applied migration; add a new migration instead.

> **Note:** startup auto-migrate validation is **best-effort** and requires the
> `migrations/` directory to be present on disk at runtime (production binaries
> often ship without the source tree, in which case startup validation is
> skipped rather than failing); authoritative enforcement is `autumn migrate
> run` / `autumn migrate status` in CI or your deploy job, which check against
> an explicit migrations directory.
>
> Startup auto-migrate only **validates** on-disk content against recorded
> hashes; it does **not** record new checksums. It applies the embedded SQL
> compiled into the binary, which may differ from the on-disk files, so
> recording those disk bytes could store a hash for content that was never
> applied. Authoritative recording happens via `autumn migrate run` and `autumn
> migrate baseline`, where the applied bytes are exactly the on-disk `up.sql`.
> An app that only ever startup-auto-migrates will therefore leave its
> migrations `unrecorded` until one of those CLI commands runs.

Line-ending and trailing-whitespace differences are normalised before hashing
(CRLF/CR/LF all collapse to LF, then `trim_end()`), so a Windows checkout and
a Linux one produce identical checksums and an editor that adds or removes a
final newline doesn't trip the guard.

### Legacy migrations: the `unrecorded` state

Migrations that were applied *before* the checksum table existed (or before
the app upgraded to a version that tracks them) show up as `unrecorded` — no
recorded checksum, no error. Record their current hashes with:

```bash
autumn migrate baseline
```

The command is additive and idempotent: it records checksums only for
applied-but-unrecorded versions, so it is safe to re-run at any time. After a
successful baseline, every applied migration is in either the `ok` or
`unrecorded` state; a subsequent edit to any of them will flip to `changed`
on the next `autumn migrate` and fail with the message above.

### Re-baseline escape hatch (deliberate edits only)

If you have deliberately edited an applied migration and accept that other
environments running the previous content will now report a mismatch, use:

```bash
autumn migrate baseline --force <version>
```

This overwrites the stored checksum for that single version with the current
on-disk hash. It is logged at `WARN` so the change is unambiguous in deploy
logs. Never the default — the `--force <version>` flag is the deliberate
signal that you understand the consequences.

### Rolling a migration back clears its checksum

Rolling a migration back (`autumn migrate down`) clears its recorded checksum,
so it can be re-applied cleanly — including with changed contents. Because the
version is no longer applied, editing its `up.sql` and re-running `autumn
migrate` records the new content's hash from scratch rather than tripping the
drift guard on a stale one.

### Checking status

`autumn migrate status` reports each applied migration's checksum state:

* `ok` — the current on-disk `up.sql` still matches the recorded hash.
* `changed` — the migration was edited after being applied; `autumn migrate`
  will refuse to run until this is resolved (add a new migration or
  re-baseline).
* `missing` — the migration is recorded as applied but its `up.sql` is gone
  (deleted or renamed after being applied); `autumn migrate` will refuse to
  run until this is resolved — add a new migration instead.
* `unrecorded` — legacy migration with no stored hash; run
  `autumn migrate baseline`.

`autumn migrate status` is **read-only**: it never creates the checksum table
and needs no write or DDL privileges, so it is safe to run against a database
with a read-only role. On a database where the checksum table does not yet
exist, every applied migration simply reports `unrecorded`. The table is created
by the framework migration (or lazily on the first `autumn migrate` /
`autumn migrate baseline` that records a hash), all of which do require
write + DDL privileges.

---

## Log output

| Level   | Event                                           |
|---------|-------------------------------------------------|
| `INFO`  | Lock key and timeout when acquisition starts   |
| `INFO`  | Lock acquired                                   |
| `DEBUG` | Waiting message (emitted every ~500 ms)         |
| `INFO`  | Lock released                                   |
| `ERROR` | Lock timeout or migration failure               |

Set `RUST_LOG=autumn_web::migrate=debug` to see the full waiting timeline.
