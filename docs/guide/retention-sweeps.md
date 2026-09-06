# Data-Retention Sweeps

Every long-running app accumulates rows nobody ever deletes: expired
sessions, old audit log entries, and the framework user's own transient
models (carts, drafts, one-time codes). Left alone, that means unbounded
table growth — index bloat, slower queries, fatter backups, and for EU apps,
a silent GDPR data-minimization gap.

`retention(...)` declares a policy on a `#[repository(...)]` and Autumn
compiles it into a batched, fleet-coordinated sweep. You write no
`#[scheduled]` fn and no SQL.

## Declare a Policy

Age-based, keyed off a timestamp column:

```rust
#[autumn_web::repository(Session, table = "sessions", retention(after = "30d", basis = created_at))]
pub trait SessionRepository {}
```

Every hour (the default interval), Autumn hard-deletes rows more than 30 days
old, measured from `created_at`.

## Soft-Delete-Aware

On a `soft_delete` repository, the same `after` policy **soft-deletes**
instead of hard-deleting — it never touches a row that is already
soft-deleted:

```rust
#[autumn_web::repository(Post, soft_delete, retention(after = "30d", basis = created_at))]
pub trait PostRepository {}
```

Add `purge_deleted_after` to hard-purge rows that have sat in the trash past
a second threshold. It requires `soft_delete` (there is no `deleted_at`
column to purge against without it) and composes with `after`:

```rust
#[autumn_web::repository(
    Post,
    soft_delete,
    retention(after = "30d", basis = created_at, purge_deleted_after = "90d")
)]
pub trait PostRepository {}
```

This gives you the full lifecycle in one attribute: live for 30 days, then
soft-deleted (still restorable, still visible via `only_deleted()`), then
hard-purged 90 days after that.

A repository with no `retention(...)` attribute behaves exactly as today —
retention is entirely opt-in.

## Options

| Option | Required | Default | Meaning |
|---|---|---|---|
| `after = "30d"` | with `basis` | — | Age threshold past which a row is stale. |
| `basis = created_at` | with `after` | — | Timestamp column age is measured from. |
| `purge_deleted_after = "90d"` | requires `soft_delete` | — | Age threshold (from `deleted_at`) past which a soft-deleted row is hard-purged. |
| `batch_size = 500` | no | `500` | Rows deleted per batch. |
| `every = "1h"` | no | `1h` | How often the sweep runs. |

Duration strings accept the same syntax as `#[scheduled(every = ...)]`:
`s`/`m`/`h`/`d`, optionally compound (`"1h 30m"`).

At least one of `after` (with `basis`) or `purge_deleted_after` must be
given; `sharded` repositories are not supported yet (a sweep would only
reach the shard it happens to be handed, silently skipping the rest), and
neither is `dependent(...)` (a sweep mutates rows directly rather than
through the cascade-aware delete path `dependent(...)` generates, so it
could orphan children or ignore an `on_delete = restrict` rule).

## `tenant_scoped` Repositories: Sweeps Are Cross-Tenant By Design

A sweep is a maintenance operation, not a request — it runs on a plain
background connection with no tenant context, so `retention(...)` on a
`tenant_scoped` repository intentionally sweeps stale rows for **every**
tenant on each run, not just one. There is no per-tenant opt-out today: if
one tenant needs a different (or no) retention window than the rest, give
that model its own repository, or exclude those rows with an additional
column-based condition your `basis`/`purge_deleted_after` filters can't
express yet.

This is a deliberate exception to the framework's usual "cross-tenant
access requires an explicit `.across_tenants()` call" rule — the same way a
DB-level TTL or a nightly VACUUM has no notion of tenant boundaries. Declare
`retention(...)` on a `tenant_scoped` model only when that's the behavior
you want.

## Batching and Multi-Replica Safety

A sweep never issues one giant `DELETE`. It walks the stale rows in
`id`-ordered batches of `batch_size` (default 500), deleting one batch at a
time, so a single run never holds a long lock or spikes replication lag. A
run stops when the table is drained or after 1000 batches — whichever comes
first — picking up where it left off on the next scheduled tick.

The sweep is registered exactly like a `#[scheduled(coordination = "fleet")]`
task, so it reuses the same [multi-replica coordination](scheduled-multi-replica.md)
guarantee: under the `postgres` scheduler backend, only one replica executes
a given sweep per tick, no matter how many replicas are running — and the
`sqlite` backend gives the same guarantee across the processes on one host.

The generated task name is `retention-sweep-<table>`, using the table name
exactly as declared — e.g. a `Session` model backed by the `sessions` table
gets `retention-sweep-sessions`. It's table-, not model-, qualified
specifically so two different models named `Session` in different modules
(`auth::Session`, `admin::Session`) can't collide on the same task name — the
schema already guarantees table names are unique, which a bare model name
isn't. (The table name is used verbatim, not lowercased: Postgres allows
distinct quoted tables that differ only in case, and lowercasing would
reintroduce the exact collision this is meant to prevent.) Task names still
share one
namespace with every `#[scheduled(name = "...")]` fn in the app; avoid
explicitly naming a hand-written task `retention-sweep-<table>` for the same
table, or the two will compete for the same coordination lock and clobber
each other's status in `/actuator/tasks`.

Table-qualifying the name doesn't fully disambiguate on its own: a table can
legitimately back more than one repository (e.g. a narrower view over the
same rows). If two *different* `#[repository(...)]` declarations both target
the same table and both declare `retention(...)`, they'd otherwise collide on
one task name and silently merge their scheduler/actuator state. The app
fails to boot instead, with an error naming the colliding task — only one
repository per table may declare `retention(...)` for now.

## Counter Caches

A retained model that is also a `#[belongs_to(Parent, counter_cache)]` child
gets the same treatment `delete_many` gives it: every row the sweep deletes
or soft-deletes decrements the parent's cached counter, in the same
transaction as the mutation. You don't do anything to opt in — declaring
both attributes on the same model is enough.

## Validate Before Enabling: Dry Run

Before deploying a new policy, see what it *would* delete without deleting
anything:

```bash
autumn retention --dry-run
autumn retention --dry-run --model Session
```

```
Model    Rows that would be swept  Duration (ms)
-----    ------------------------  -------------
Post     12                        4
Session  238                       9
```

This runs the app binary against the real (development) database, counts the
matching rows for every declared policy, and prints a report — nothing is
deleted.

## Observability

Every real sweep run emits a structured log line and, unless it's a dry run,
bumps two metrics — both labeled by `model`:

- `retention_sweep_rows_total` (counter) — cumulative rows swept.
- `retention_sweep_duration_seconds` (timer/histogram) — how long each run took.

Both show up at `/actuator/prometheus` and `/actuator/metrics` alongside your
own [app-defined metrics](metrics.md).

## See Also

- [Soft Delete](soft-delete.md) — the `deleted_at` / `purge()` / `only_deleted()`
  mechanism `purge_deleted_after` builds on.
- [Multi-Replica Scheduled Tasks](scheduled-multi-replica.md) — the
  coordination guarantee retention sweeps reuse.
- The `gdpr` module (`autumn_web::gdpr`) — request-driven subject
  erasure/export. Retention sweeps are the opposite: proactive, scheduled,
  and app-declared rather than triggered by a user request.
- [Data Retention for Framework-Owned Data](data-retention.md) — the
  counterpart for Autumn's *own* tables and stores (job history, idempotency,
  experiment assignments, webhook replay, sessions, audit archives), declared
  in one `[retention]` config section rather than per repository.
