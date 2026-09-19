# Soft Delete

Soft delete lets you mark records as deleted (by stamping a `deleted_at` timestamp)
rather than removing rows from the database. Deleted rows are hidden from all default
finders but can be restored or permanently purged later.

## Model setup

Add a `deleted_at` column to your model and migration:

```rust
#[autumn_web::model]
pub struct Article {
    #[id]
    pub id: i64,
    pub title: String,
    pub deleted_at: Option<chrono::NaiveDateTime>,
}
```

In your `up.sql` migration:

```sql
CREATE TABLE articles (
    id BIGSERIAL PRIMARY KEY,
    title TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT NOW(),
    deleted_at TIMESTAMP NULL
);
```

And in `schema.rs`:

```rust
diesel::table! {
    articles (id) {
        id -> Int8,
        title -> Text,
        created_at -> Timestamp,
        deleted_at -> Nullable<Timestamp>,
    }
}
```

## Repository annotation

Pass `soft_delete` to the `#[repository]` attribute:

```rust
#[autumn_web::repository(Article, soft_delete)]
pub trait ArticleRepository {}
```

The `soft_delete` flag:

- Changes `delete_by_id(id)` to `UPDATE articles SET deleted_at = now() WHERE id = ?`
- Adds a `WHERE deleted_at IS NULL` filter to `find_by_id`, `find_all`, `count`,
  `exists_by_id`, `page`, and all derived `find_by_*` / `count_by_*` queries
- Generates four extra methods on the trait:

| Method | Description |
|--------|-------------|
| `restore(id)` | Sets `deleted_at = NULL` (un-deletes the record) |
| `purge(id)` | Issues a hard `DELETE FROM` — permanent |
| `with_deleted()` | Returns all records, including soft-deleted ones |
| `only_deleted()` | Returns only records where `deleted_at IS NOT NULL` |

## Generator support

The `autumn generate` CLI accepts a `--soft-delete` flag:

```shell
autumn generate model Article title:String --soft-delete
autumn generate scaffold Article title:String --soft-delete
```

This automatically adds the `deleted_at` field, the nullable column in the migration,
the `Nullable<Timestamp>` entry in `schema.rs`, and the `soft_delete` annotation in
the generated repository file.

### Scaffolded Trash view

A `--soft-delete` **scaffold** additionally generates the recover-from-trash
UI, so nothing about the flow is hand-written:

| Generated | What it does |
| --------- | ------------ |
| `GET /<plural>/trash` | Lists deleted rows through the repository's `page_only_deleted` — the paginated `only_deleted` scope. Linked from the index as **Trash**. |
| `POST /<plural>/{id}/restore` | CSRF-protected **Restore** button per row; calls `restore(id)`, flashes, redirects back to Trash. |
| `POST /<plural>/{id}/purge` | CSRF-protected **Purge** button per row behind a server-rendered confirm dialog; calls `purge(id)` — the only hard delete in the app. |
| `tests/<name>.rs` | A lifecycle test: create → delete → in Trash / out of index → restore → purge. |

Both write handlers load their row with `deleted_at IS NOT NULL` before acting,
so a request for a row that is not in the trash 404s rather than purging a live
record. The trash surface is emitted only on the standard HTML path — see
[generators](./generators.md#trash-restore-and-purge---soft-delete) for the
gated-off variants and the reasoning.

## Lifecycle example

```rust
// Soft-delete (sets deleted_at = now())
repo.delete_by_id(42).await?;

// Record is now invisible to standard finders
assert!(repo.find_by_id(42).await?.is_none());

// Inspect the trash
let trashed = repo.only_deleted().await?;

// Un-delete
repo.restore(42).await?;
assert!(repo.find_by_id(42).await?.is_some());

// Hard-delete permanently
repo.purge(42).await?;
```

## Combining with hooks

`soft_delete` is compatible with `hooks = MyHooks`. The `before_delete` /
`after_delete_commit` hooks fire on soft-deletes the same way they do on hard
deletes. `purge` does not invoke hooks — it issues a direct `DELETE FROM`.

## Admin panel (Trash tab)

Models registered with the admin plugin can expose a **Trash** tab by implementing
the soft-delete methods on `AdminModel`:

```rust
impl AdminModel for ArticleAdmin {
    fn supports_soft_delete(&self) -> bool { true }

    fn restore<'a>(&'a self, pool: &'a Pool<AsyncPgConnection>, id: i64) -> AdminFuture<'a, ()> {
        Box::pin(async move {
            let repo = PgArticleRepository { pool: pool.clone() };
            repo.restore(id).await.map_err(|e| AdminError::Other(e.to_string()))
        })
    }

    fn purge<'a>(&'a self, pool: &'a Pool<AsyncPgConnection>, id: i64) -> AdminFuture<'a, ()> {
        Box::pin(async move {
            let repo = PgArticleRepository { pool: pool.clone() };
            repo.purge(id).await.map_err(|e| AdminError::Other(e.to_string()))
        })
    }

    fn list_deleted<'a>(
        &'a self,
        pool: &'a Pool<AsyncPgConnection>,
        params: ListParams,
    ) -> AdminFuture<'a, ListResult> {
        // ... query only_deleted() and map to ListResult
    }
}
```

The default `execute_action` implementation already dispatches `"restore"` and
`"purge"` bulk action names to the methods above, so no custom `execute_action`
override is needed unless you want additional bulk actions.
