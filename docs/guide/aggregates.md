# Aggregate Queries

Dashboards and stats pages rarely want individual rows — they want *roll-ups*:
how many bookmarks per tag, the total score per post, the average order value per
customer, a count of sign-ups per day. Autumn expresses these `GROUP BY`
aggregates **declaratively on a `#[repository]` trait**: you name a method like
`count_grouped_by_tag`, and the macro generates a typed, lazy query builder that
runs the `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` in the database and hands you back a
`Vec<(key, value)>`.

No hand-written SQL, no loading every row and folding in memory, and no untyped
`sql_query` — the group key and aggregate value are real Rust types the compiler
checks. The generated query also composes the repository's soft-delete filter,
tenant scoping, and read-replica routing exactly like a plain `count`, so a
roll-up inherits all of that for free.

The full API lives in [`autumn_web::aggregate`]; this guide builds it up against
the [`bookmarks`](../../examples/bookmarks) example, whose `GET /stats` route runs
two real roll-ups.

## Prerequisites

Grouped aggregates ride on `#[autumn_web::repository]`, so you need the default
Postgres/Diesel repository stack — the same setup any Autumn app with a
`#[model]` and a `#[repository]` already has. Nothing extra to enable: the
`aggregate` module is part of the core `autumn-web` crate.

```toml
[dependencies]
autumn-web = { version = "0.7", features = ["openapi"] }
```

The `DateBucket` type used for time-series roll-ups is at
`autumn_web::aggregate::DateBucket`; the builder types are re-exported from the
same module.

## Declaring a roll-up

Aggregates are declared as extra methods on your existing `#[repository]` trait.
The **method name is the query** — the macro parses it into a grouped aggregate:

- `count_grouped_by_<col>` → `COUNT(*) GROUP BY <col>`
- `sum_<col>_grouped_by_<group_col>` → `SUM(<col>) GROUP BY <group_col>`
- `avg_<col>_` / `min_<col>_` / `max_<col>_grouped_by_<group_col>` likewise

The return type `Vec<(K, V)>` names the group-key type `K` and the aggregate
value type `V`. In the bookmarks example (`src/repositories/bookmark.rs`) we add
two:

```rust,ignore
#[autumn_web::repository(Bookmark, api = "/api/bookmarks")]
pub trait BookmarkRepository {
    fn find_by_tag(tag: String) -> Vec<Bookmark>;
    fn find_by_alive(alive: bool) -> Vec<Bookmark>;

    /// `COUNT(*) GROUP BY tag` → one `(tag, count)` pair per distinct tag.
    fn count_grouped_by_tag() -> Vec<(String, i64)>;
    /// `COUNT(*) GROUP BY created_at` → raw per-timestamp counts, later
    /// rolled into a daily series with `.bucket(DateBucket::Day)`.
    fn count_grouped_by_created_at() -> Vec<(chrono::NaiveDateTime, i64)>;
}
```

Each declared method becomes an **inherent method** on the generated
`PgBookmarkRepository` struct that returns a lazy
[`GroupedAggregate`](autumn_web::aggregate::GroupedAggregate) builder (mirroring
`find_in_batches`). Nothing touches the database until you call the terminal
`.load()`.

## Running the query

Call the generated method, then `.load().await` to execute:

```rust,ignore
// COUNT(*) GROUP BY tag → Vec<(tag, count)>, one pair per distinct tag.
let by_tag: Vec<(String, i64)> = repo.count_grouped_by_tag().load().await?;
```

The result is one `(key, value)` pair per group. An empty table yields an empty
`Vec`; the database decides the group order unless you ask for one (next
section).

`COUNT` values are a plain `i64`. `SUM`/`AVG`/`MIN`/`MAX` are **null-safe** and
so wrap their value in `Option`: a group whose aggregated column is entirely
`NULL` comes back as `(key, None)`, never a fabricated `Some(0)`. See
[Value and key types](#value-and-key-types) for the full table.

## Top-N: order and limit

To rank groups — the classic "top 10 tags" — order by the aggregated value and
cap the result. Both are chainable builder methods, and the ordering/limit run in
the database (you get exactly N rows back, not the whole set sorted in Rust):

```rust,ignore
// Top 10 tags by bookmark count, largest first.
let top_tags: Vec<(String, i64)> = repo
    .count_grouped_by_tag()
    .order_by_aggregate_desc()
    .limit(10)
    .load()
    .await?;
```

Use [`order_by_aggregate_asc`](autumn_web::aggregate::GroupedAggregate::order_by_aggregate_asc)
for smallest-first. The ordering is on the *aggregate value*, not the group key —
that is what makes it a leaderboard rather than an alphabetical list.

## Filtering before the group

[`filter_eq`](autumn_web::aggregate::GroupedAggregate::filter_eq) and
[`filter_range`](autumn_web::aggregate::GroupedAggregate::filter_range) scope
which rows feed the aggregate — they apply **before** grouping, and both bounds
are bound as query parameters (never string-interpolated):

```rust,ignore
// Only bookmarks created in a window feed the counts.
let recent: Vec<(chrono::NaiveDateTime, i64)> = repo
    .count_grouped_by_created_at()
    .filter_range(window_start, window_end)
    .load()
    .await?;
```

The predicate is always on the **raw** group column, so a range over timestamps
windows the input to a time-series roll-up.

## Time series with `DateBucket`

Grouping on a raw `created_at` timestamp gives one bucket per distinct instant —
almost never what you want. [`bucket`](autumn_web::aggregate::GroupedAggregate::bucket)
groups by `date_trunc('<unit>', <col>)` instead, collapsing the timestamps into
`Day`, `Week`, or `Month` buckets keyed by each bucket's start:

```rust,ignore
use autumn_web::aggregate::DateBucket;

let window_end = chrono::Utc::now().naive_utc();
let window_start = window_end - chrono::Duration::days(30);

// Bookmarks added per day over the trailing 30 days.
let mut per_day: Vec<(chrono::NaiveDateTime, i64)> = repo
    .count_grouped_by_created_at()
    .bucket(DateBucket::Day)
    .filter_range(window_start, window_end)
    .load()
    .await?;

// The database groups in no defined order; sort into a chronological series.
per_day.sort_by_key(|(day, _)| *day);
```

`.bucket()` is **type-gated**: it exists only when the group key is a timestamp
type (`NaiveDateTime` for `timestamp`, `DateTime<Utc>` for `timestamptz`). A
non-temporal key — like the `String` tag from `count_grouped_by_tag` — has no
`.bucket()` method at all, so an invalid `date_trunc` on a non-timestamp column is
a compile error rather than a runtime failure. As shown above, `filter_range`
still windows the **raw** timestamps, which is exactly what bounds a bucketed
series.

## Value and key types

The macro reads `K` and `V` straight from the declared `Vec<(K, V)>` and bakes in
the matching Postgres types:

| method                 | `V`                              |
|------------------------|----------------------------------|
| `count_grouped_by_`    | `i64`                            |
| `sum_*_grouped_by_`    | `Option<T>` (`T` = column type)  |
| `min_*` / `max_*`      | `Option<T>`                      |
| `avg_*_grouped_by_`    | `Option<f64>`                    |

`K` is the group column's Rust type (or, under `.bucket()`, the bucket-start
timestamp). A nullable group-key **type** (`K = Option<T>`) is rejected at
compile time; a nullable group **column** is fine — rows with a `NULL` key are
simply excluded (the generated SQL guards with `IS NOT NULL`). Nullable *value*
columns are fine and drive the `→ None` behaviour above.

## Scoping comes for free

A grouped aggregate acquires its connection through the same read-route helper as
`count`, and composes the same predicates:

- **Soft delete** — a `soft_delete` repository excludes trashed rows from the
  aggregate (`deleted_at IS NULL`), matching every other finder.
- **Multi-tenancy** — a `tenant_scoped` repository only aggregates the active
  tenant's rows. (Because `SUM`/`AVG`/`MIN`/`MAX` cannot be merged across shards,
  a sharded tenant repository used via `across_tenants()` rejects grouped
  aggregates rather than returning a per-shard-partial answer.)
- **Read replicas** — the query routes to the read role, so a replica-routed
  repository rolls up off the replica.

## Encrypted columns

Grouped aggregates are **not** available on an `#[encrypted(...)]` column, as
either the group key or the aggregated value: the column stores ciphertext, so
grouping would bucket by ciphertext and an equality filter would compare
plaintext against ciphertext and match nothing. Such a method returns an error at
call time — group on (or aggregate over) a non-encrypted column, or drop to a raw
query.

## Try it in the bookmarks example

The [`bookmarks`](../../examples/bookmarks) example wires both roll-ups into a
single `GET /bookmarks/stats` route (in `src/routes/bookmarks.rs`) that renders a
**Top tags** table (`count_grouped_by_tag` +
`.order_by_aggregate_desc().limit(...)`) and an **Added per day** series
(`count_grouped_by_created_at` + `.bucket(DateBucket::Day).filter_range(...)`).
With the app running and some seeded data:

```bash
# Populate faked bookmarks, then open the stats page.
autumn seed --count 200 --model Bookmark
curl -fsS http://localhost:3000/bookmarks/stats
```

## When *not* to reach for a grouped aggregate

- **A single scalar** — a plain total or one average across the whole table is a
  `count`/`sum` on the repository, not a `GROUP BY`.
- **Per-row data you then group in the template** — if you need the individual
  rows anyway, load them once and group in Rust rather than issuing a second
  aggregate query.
- **Multi-column composite keys or `HAVING`** — the declarative form groups on a
  single column; a genuinely multi-dimensional cube wants a raw query.

Grouped aggregates earn their keep exactly when you want *one typed roll-up per
group*, computed in the database, with soft-delete/tenant/replica scoping applied
automatically.
