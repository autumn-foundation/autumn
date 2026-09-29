# Record Version History

Autumn's version history feature automatically captures an immutable audit
trail of every write to opted-in `#[repository]` models. It answers the
question _"who changed this row, when, and to what?"_ without any per-call-site
instrumentation.

## When to use this vs. `audit::record` (S-057)

| Concern | Tool |
|---------|------|
| "Who changed row 42's `plan_tier`, and what was the previous value?" | **Version history** (this guide) |
| "Which admin exported user data at 14:32?" | `autumn::audit` (S-057) |
| "Was this action authorised?" | `#[authorize]` / `Policy` |

Version history is for **row state over time**. S-057 audit logging is for
**named business actions** (`user.role.update`, `data.export`). They coexist:
use both when a row mutation is also a named compliance event.

## Opting in

Add `versioned = true` to the `#[repository]` attribute:

```rust
#[repository(Post, versioned = true)]
pub trait PostRepository {}
```

That is the **only per-model change required**. Every write path — hand-written
handlers, `#[repository(api = "…")]` auto-generated endpoints, `#[job]` and
`#[mailer]` paths, and `autumn task` one-off scripts — captures history
automatically.

## Migration

Run `autumn migrate` (or `cargo run -- migrate`) after opting a model in.
The first time you add `versioned = true` to any model, Autumn runs the
framework migration that creates `_autumn_version_history`:

```sql
CREATE TABLE _autumn_version_history (
    id          BIGSERIAL   PRIMARY KEY,
    table_name  TEXT        NOT NULL,
    tenant_id   TEXT,
    record_id   BIGINT      NOT NULL,
    op          TEXT        NOT NULL CHECK (op IN ('insert', 'update', 'delete')),
    actor       TEXT        NOT NULL DEFAULT 'system',
    request_id  TEXT,
    changes     JSONB       NOT NULL DEFAULT '[]',
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

Adding version history to a model **after launch** is non-destructive: the
migration appends rows going forward; existing rows are not backfilled (the
table starts empty for that model). This is by design — retroactive history is
unknowable.

## Retrieving history

The generated repository exposes a `version_history` method:

```rust
use autumn_web::version_history::VersionFilter;

// First page, 25 entries per page (default filter).
let page = repo.version_history(post_id, VersionFilter::default()).await?;

// Compliance query: changes to post 42 between two timestamps.
let filter = VersionFilter::between(
    "2026-04-15T00:00:00Z".parse()?,
    "2026-04-15T23:59:59Z".parse()?,
);
let page = repo.version_history(42, filter).await?;

for entry in &page.entries {
    println!(
        "{} changed {} at {} by {} (request {})",
        entry.op,
        entry.table_name,
        entry.recorded_at,
        entry.actor,
        entry.request_id.as_deref().unwrap_or("—"),
    );
    for change in &entry.changes {
        if change.sensitive {
            println!("  {} [sensitive — value omitted]", change.column);
        } else {
            println!(
                "  {} : {:?} → {:?}",
                change.column, change.before, change.after
            );
        }
    }
}
```

Entries are returned in chronological order (oldest first).

## Actor resolution

| Context | Actor value |
|---------|-------------|
| Authenticated HTTP request | `session["user_id"]` (Autumn's default auth key) |
| No session (job, task, migration) | `"system"` |
| Explicitly set in `MutationContext` | `ctx.actor = Some("service-account".into())` |

## Sensitive columns

Exclude columns from the captured diff via a `version_history` annotation on
the repository:

```rust
#[version_history(sensitive = ["password_digest", "reset_token"])]
#[repository(Post, versioned = true)]
pub trait PostRepository {}
```

Excluded columns **still appear** in the diff as changed (so the timeline
remains complete and compliance evidence is not lost) — but their `before` and
`after` values are replaced with `null` and `sensitive: true` is set:

```json
{ "column": "password_digest", "before": null, "after": null, "sensitive": true }
```

This means you can answer "was this column changed?" without leaking the value.

## Admin panel History pane

When `has_history()` returns `true` on an `AdminModel` implementation (which
`versioned = true` sets automatically), the admin panel shows a **History** tab
on every detail page. No route configuration is required from the app.

The History pane lists entries with actor, timestamp, and column-level diff.

## Performance characteristics

The version history write path:

1. Serializes the model to JSON (one `serde_json::to_value` call).
2. Computes the column-level diff (one JSON object comparison).
3. Inserts one row into `_autumn_version_history` in the **same transaction**
   as the mutating statement.

The additional round-trip is a single `INSERT` in the same already-open
transaction, so it does not add a full network RTT. The in-process overhead
(`compute_diff`) runs in single-digit microseconds (see
`cargo bench -p autumn-web --bench version_history`).

**Budget**: the feature must not regress p99 write latency by more than 5 ms
relative to the same repository with version history off.

## Computing a diff yourself

Step 2 of the write path above is available directly, in two shapes:

| What you have                        | What to call |
|--------------------------------------|--------------|
| a `Value` you still need afterwards  | `compute_diff`, `compute_insert_changes`, `compute_delete_changes` |
| a `Value` you are done with          | `compute_diff_owned`, `compute_insert_changes_owned`, `compute_delete_changes_owned` |

Both shapes produce the identical changeset. The `_owned` variants take the
`serde_json::Value` by value so every retained column name and value is *moved*
into its `ColumnChange` rather than cloned, which is why the generated write
path uses them: it serializes a throwaway `Value` per mutation and drops it
immediately after the diff.

```rust,ignore
use autumn_web::version_history::compute_diff_owned;

let changes = compute_diff_owned(before_json, after_json, &["password_digest"]);
```

Reach for the borrowed variants when the `Value` has to outlive the call.

## Storage growth

Each row in `_autumn_version_history` is roughly proportional to the number of
changed columns and their value sizes. A typical row-level update capturing
2–3 changed text columns is on the order of 200–500 bytes.

Growth is bounded by your write rate: a model receiving 100 writes/second will
accumulate roughly 8.6 million entries per day. Plan your retention policy
accordingly. Autumn does not manage retention; apps own their data lifecycle.

To keep the history table small in high-volume scenarios:

- Use `pg_partman` to partition `_autumn_version_history` by `recorded_at`.
- Archive old partitions to cold storage.
- Run a periodic `DELETE FROM _autumn_version_history WHERE recorded_at < now() - interval '90 days'`
  via an `#[scheduled]` task.

## Immutability guarantee

There is **no public framework method to update or delete history entries**.
The `_autumn_version_history` table is append-only at the application API
level.

Test-fixture teardown uses `VersionHistoryStore::__test_clear_for_record`,
which is explicitly marked `#[doc(hidden)]` and not part of the stable public
API. Do not use it in production code.

## Repositories without version history

Repositories that do not opt in (`versioned = true` absent) compile, run, and
migrate unchanged. This feature is entirely additive: downstream apps on prior
versions do not break on upgrade.

## Example (blog)

The `examples/blog` example demonstrates the History pane by implementing
`has_history()` and `get_history()` manually on `PostAdmin`. In an application
using `#[repository]`, both methods are generated automatically.

```rust
// In examples/blog/src/admin.rs — illustrating the contract
impl AdminModel for PostAdmin {
    fn has_history(&self) -> bool { true }

    fn get_history<'a>(
        &'a self,
        pool: &'a Pool<AsyncPgConnection>,
        record_id: i64,
        page: u64,
        per_page: u64,
    ) -> AdminFuture<'a, AdminHistoryPage> {
        // In a #[repository(versioned = true)] app, call:
        //   repo.version_history(record_id, VersionFilter { page, per_page, ..Default::default() })
        // and map VersionPage → AdminHistoryPage.
        todo!("delegate to PgPostRepository::version_history")
    }
}
```
