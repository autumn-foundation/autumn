# Pagination

Autumn ships two complementary pagination flavours, both available out of the
box in every `#[repository]`-backed resource and in scaffold-generated index
views:

| Flavour | Query params | Best for |
|---------|-------------|----------|
| **Offset** (`PageRequest` / `Page`) | `?page=N&size=M` | Browse-style UIs, admin tables |
| **Cursor** (`CursorRequest` / `CursorPage`) | `?cursor=<token>&size=M` | Feeds, large tables, replicas |

---

## Offset pagination

### How it works

Offset pagination uses a `LIMIT` / `OFFSET` SQL pair.  The client picks a
1-based page number (`?page=2`) and a page size (`?size=25`).  The server
executes two queries — `COUNT(*)` and the page slice — and returns a `Page<T>`
response that bundles the content together with total-pages metadata.

### In a `#[repository]` trait

Every `#[repository]`-generated struct gets a `page` method automatically:

```rust
// Defined in your repository trait (generated):
async fn page(&self, req: &PageRequest) -> AutumnResult<Page<Post>>;
```

Call it from any handler:

```rust
use autumn_web::pagination::{Page, PageRequest};
use crate::repositories::post::PgPostRepository;

#[get("/posts")]
async fn index(page: PageRequest, repo: PgPostRepository) -> AutumnResult<Json<Page<Post>>> {
    Ok(Json(repo.page(&page).await?))
}
```

### In a scaffold-generated index view

`autumn generate scaffold Post title:String body:Text` emits an `index` action
that uses the `PageRequest` extractor to call `repo.page()`.  Out-of-range or
missing values are clamped silently — consistent with the framework rule that
list endpoints never return HTTP 400 for pagination parameters:

```
GET /posts          → page 1, 20 items  (DEFAULT_PAGE_SIZE)
GET /posts?page=3   → page 3, 20 items
GET /posts?size=10  → page 1, 10 items
GET /posts?size=200 → page 1, 100 items (clamped to MAX_PAGE_SIZE)
GET /posts?size=abc → page 1, 20 items  (unparseable → default)
```

A Maud `pagination_nav` helper renders Previous / Next links with `hx-get`
attributes for htmx-friendly partial updates — see
[Rendering the pager](#rendering-the-pager) below.

### Overriding page size

`PageRequest` uses `DEFAULT_PAGE_SIZE = 20` and `MAX_PAGE_SIZE = 100`.  Both
are public constants you can reference in your own code:

```rust
use autumn_web::pagination::{DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE, PageRequest};

let req = PageRequest::new(1, 50); // page 1, 50 items
```

Values outside the valid range are clamped silently — `PageRequest` never
returns HTTP 400 on its own.

### Response shape

```json
{
  "content": [ ... ],
  "page": 1,
  "size": 25,
  "total_elements": 137,
  "total_pages": 6,
  "has_next": true,
  "has_previous": false
}
```

---

## Cursor pagination

### When to use it

Cursor (keyset) pagination is O(1) regardless of page depth and produces
**zero duplicate or skipped rows** under concurrent inserts — making it the
correct choice for:

- Real-time feeds and notification inboxes
- Admin-safe full-table iteration (exports, data migrations)
- Apps running on multiple read replicas where `OFFSET` can diverge

### Declaring a cursor key

Add `cursor_key = field` to the `#[repository]` attribute to generate the
`cursor_page` method.  The field is used as the primary sort column (descending)
with `id` as the tie-breaker:

```rust
#[autumn_web::repository(Post, cursor_key = created_at)]
pub trait PostRepository {}
```

This generates a `cursor_page` method that orders by `(created_at DESC, id DESC)`
and uses `id` as the sole cursor payload — correct whenever `created_at` values
are monotonically correlated with `id` (the typical case for auto-increment PKs).

For **universally correct** keyset pagination (e.g. backfilled or imported rows
where timestamps and ids may diverge), also supply `cursor_key_type`:

```rust
#[autumn_web::repository(Post, cursor_key = created_at, cursor_key_type = chrono::NaiveDateTime)]
pub trait PostRepository {}
```

With `cursor_key_type` the cursor encodes both `(NaiveDateTime, i64)` and the
WHERE clause becomes the full two-part predicate:
```sql
WHERE (created_at < $after_k) OR (created_at = $after_k AND id < $after_id)
```

> **Constraint:** `cursor_key` must be declared on a **non-nullable** column.
> In SQL, comparisons involving `NULL` (`<`, `=`) evaluate to `UNKNOWN`, so
> a nullable sort key silently drops rows from all keyset pagination queries.
> Make the column `NOT NULL` or implement `cursor_page` manually.

### Calling `cursor_page`

```rust
use autumn_web::pagination::{CursorPage, CursorRequest};
use crate::repositories::post::PgPostRepository;

#[get("/feed")]
async fn feed(cursor: CursorRequest, repo: PgPostRepository) -> AutumnResult<Json<CursorPage<Post>>> {
    Ok(Json(repo.cursor_page(&cursor).await?))
}
```

The first request omits `?cursor`; subsequent requests pass the `next_cursor`
token returned by the previous response.

### Cursor token format

Cursor tokens are base64url-encoded JSON, URL-safe without percent-encoding,
and opaque to clients.  Forging a token is equivalent to seeking to an
arbitrary offset — for sort-key-only cursors (timestamps + ids) this is not a
security concern.  If your cursor encodes **sensitive data** (tenant ids, access
scopes) use the signed cursor API:

```rust
// Encoding
let token = Cursor::encode_signed(&my_value, signing_key);

// Decoding in a handler
let value = cursor_req.decode_signed::<MyValue>(signing_key);
```

See the [`pagination`](https://docs.rs/autumn-web/latest/autumn_web/pagination/index.html)
module docs for the full signing API.

### Response shape

```json
{
  "content": [ ... ],
  "size": 20,
  "next_cursor": "eyJpZCI6MTIzfQ",
  "has_next": true
}
```

`next_cursor` is `null` on the final page.

---

## Batched iteration (server-side, bounded memory)

Cursor pagination shapes an HTTP *response*. When you instead need to touch
*every* row of a table from inside a `#[autumn_web::task]`, a scheduled sweep,
or a job — a backfill, a counter-cache recompute, a re-encryption pass — reach
for **batched iteration** instead of `find_all()`. `find_all()` materializes
the whole table into one `Vec` (a 1 M-row table is an instant OOM); the batched
iterators walk the table in bounded-size chunks with flat memory.

Every `#[repository]` generates two methods:

- `find_in_batches(batch_size)` — yields successive `Vec<Model>` chunks of at
  most `batch_size` rows.
- `find_each(batch_size)` — yields one `Model` at a time, still fetching under
  the hood in `batch_size`-sized batches.

Iteration is keyset-based (primary-key ascending: `WHERE id > last ORDER BY id
ASC LIMIT batch_size`), not `LIMIT`/`OFFSET`, so deep iteration never degrades
and is stable under concurrent inserts. At most one `batch_size` chunk of
models is resident at a time — memory is `O(batch_size)`, never `O(table)`.
Unlike a `cursor_page` request, `batch_size` is **not** clamped to
`MAX_PAGE_SIZE` (100); pass whatever fits your memory budget.

Batched iteration inherits the repository's soft-delete filter (trashed rows
are skipped, matching `find_all`) and its read routing (a replica-routed repo
iterates off the replica, a `primary_reads` repo off the primary), so backfills
read off a replica with no extra ceremony. An error mid-iteration surfaces as
an `AutumnResult` error on the failing batch, and errors are **retryable**:
the keyset cursor only advances on success, so calling `next_batch()` again
retries the same batch with no duplicated or skipped rows — `Ok(None)` always
means the table is exhausted, never a swallowed failure.

Iteration ends at the first short batch (fewer than `batch_size` rows); rows
inserted after that point are not seen by the current handle — start a new
iteration to pick them up (matching Rails' `find_each`).

### `find_each` in a task — per-row update

```rust
#[autumn_web::task(name = "backfill-slugs")]
pub async fn backfill_slugs(repo: PgPostRepository) -> AutumnResult<()> {
    let mut each = repo.find_each(500);
    while let Some(post) = each.next().await? {
        let slug = slugify(&post.title);
        repo.update(
            post.id,
            &UpdatePost {
                slug: Patch::Set(slug),
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(())
}
```

Register it with `.one_off_tasks(one_off_tasks![tasks::backfill_slugs])` and run
`autumn task backfill-slugs` (see the [tasks guide](./tasks.md)).

### `find_in_batches` + `upsert_many` — recompute loop

Pair batched reads with the bulk writes from
[`upsert_many`](./repositories.md) to recompute a whole table with flat memory
on both the read and write side (`upsert_many` takes existing `&[Model]`
values and writes them back; `save_many` takes `&[NewModel]` and would insert
duplicates here):

```rust
let mut batches = repo.find_in_batches(1_000);
while let Some(chunk) = batches.next_batch().await? {
    let recomputed: Vec<Account> = chunk
        .into_iter()
        .map(|mut account| {
            account.balance = recompute_balance(&account);
            account
        })
        .collect();
    repo.upsert_many(&recomputed).await?; // O(batch_size) memory, not O(table)
}
```

Note: `upsert_many` is not generated on repositories with hooks configured —
use per-row `update` there.

A `batch_size` of `0` returns an error rather than spinning. On a **sharded**
repository, cross-shard `across_tenants()` iteration is rejected (mirroring
`cursor_page`); iterate each shard separately via `from_shard(...)` instead.

---

## Offset vs cursor: decision guide

| Question | Use offset | Use cursor |
|---------|-----------|-----------|
| Do users navigate to a specific page number? | ✓ | |
| Is the list mostly stable (rarely updated)? | ✓ | |
| Is the list a feed with concurrent inserts? | | ✓ |
| Is the table > 1 M rows? | | ✓ |
| Do you run read replicas? | | ✓ |
| Do you need infinite scroll / exports? | | ✓ |

---

## Sorting and filtering: `ListQuery`

Pagination rarely arrives alone. A list view usually also wants "sort by title
descending" and "only the published ones" — and that is where list endpoints
grow SQL injection holes, because the column name comes from the query string.

`ListQuery` is the extractor for those parameters:

| Parameter | Meaning | Default |
|---|---|---|
| `sort` | column key to order by | the model's default order |
| `dir` | `asc` or `desc` | `asc` — anything unrecognized falls back to `asc` |
| `filter[<col>]` | equality filter on column `<col>` | none |

It composes with `PageRequest`, and the generated repository `list()` applies
both in one call:

```rust,ignore
use autumn_web::prelude::*;   // ListQuery, PageRequest, Page, pagination_nav

// GET /posts?sort=title&dir=desc&filter[published]=true&page=2&size=25
#[get("/posts")]
async fn index(
    list_query: ListQuery,
    page_req: PageRequest,
    repo: PgPostRepository,
) -> AutumnResult<Json<Page<Post>>> {
    Ok(Json(repo.list(&list_query, &page_req).await?))
}
```

### The allowlist is the security boundary

`ListQuery` itself validates **nothing**. It carries the raw request intent and
nothing more. The guarantee lives in the generated `list()`, which matches each
requested key against the model's own columns through Diesel's typed DSL:

- only the model's columns are sortable;
- only its non-null `String` / integer / `bool` columns are filterable;
- **any other key hits the default arm and is silently ignored** — it can never
  be interpolated into SQL.

That is why `?sort=id;DROP TABLE users` is inert rather than dangerous:
`id;DROP TABLE users` is not a column, so the query falls back to the model's
default ordering. There is no escaping step to get wrong, because no user text
ever reaches the SQL string.

Filters are applied to the `COUNT` query **and** the page query, so `total` and
`total_pages` describe the filtered set rather than the whole table.

### It never rejects a request

Like `PageRequest`, `ListQuery`'s extraction is infallible. An empty `sort`
falls back to the default order, an unrecognized `dir` falls back to `asc`, and
unknown parameters are dropped. A malformed list URL — from a stale bookmark, a
crawler, or a hand-edited address bar — renders the list rather than a 400.

### Sortable table headers

The `data_table` widget renders header links that toggle `sort`/`dir` and carry
the matching `aria-sort` attribute, preserving the rest of the query string. It
shares the `SortDir` type with `ListQuery`, so the request half and the view
half cannot drift:

```rust,ignore
use autumn_web::widgets::{Column, DataTableConfig, data_table};

data_table(&page.content, &columns, &DataTableConfig {
    caption: Some("Posts"),
    empty_message: "No posts yet.",
    base_path: "/posts",
    query: &raw_query,      // the current request's query string
    ..Default::default()
})
```

---

## Costs, and how to keep them down

Pagination is where a fast page quietly becomes a slow one. Four things account
for most of it.

**`COUNT(*)` is not free.** Offset pagination runs two queries, and on a large
table the count is usually the expensive one. If the total exists only to render
"page 3 of 412", consider whether the UI needs it — a cursor feed does not run
it at all.

**Deep offsets scan.** `OFFSET 50000` makes the database walk fifty thousand
rows before discarding them. The cost grows with the page number, so the last
page of a big list is the slowest request on the site. Cursor pagination is
O(1) in page depth; that is the reason to prefer it for large tables, not
fashion.

**Offset pages are not stable under concurrent inserts.** A row inserted at the
head while a user reads page 1 pushes one row from page 1 onto page 2, so they
see it twice — or, on a delete, never see it at all. For a feed, that is a bug
report. Cursor pagination is keyset-based and does not have the problem.

**Two extractors, two connections.** A handler that holds `Db` *and* takes a
repository extractor holds two pooled connections at once. Under the default
ten-connection pool, ten such concurrent requests deadlock. Drop the first
before acquiring the second:

```rust,ignore
let total: i64 = posts::table.count().get_result(&mut db).await?;
let items = load_page(&mut db, &page_req).await?;
drop(db);                       // release before the repository checks out
let extras = repo.something_else().await?;
```

Two more things worth doing once:

- **Cap the page size.** `size` is already clamped to `MAX_PAGE_SIZE`, so
  `?size=100000` cannot turn a list route into a denial-of-service vector. Do
  not undo that clamp by reading `size` yourself.
- **Give paginated pages a canonical URL.** Page 2 of a list is not a duplicate
  of page 1, and self-referential canonicals on each page are the right answer.
  See the [SEO guide](./seo.md).

---

## htmx wiring

Scaffold-generated pagination links carry `hx-get` and `hx-target="body"`
attributes so htmx replaces the full page body on click — no additional JS
needed.  For partial updates (replacing only the list, not the full layout),
change `hx-target` to the id of your list container and set `hx-swap="innerHTML"`.

Example fragment from a generated `index.html`:

```html
<a href="/posts?page=2&size=25"
   hx-get="/posts?page=2&size=25"
   hx-target="body">
  Next →
</a>
```

---

## Rendering the pager

You don't have to hand-roll that markup. Autumn ships a reusable Maud renderer,
[`pagination_nav`], that turns a `Page` into an accessible, filter-preserving,
htmx-ready pager in one line. It is re-exported from the prelude alongside the
other view widgets, so `use autumn_web::prelude::*;` brings it into scope.

```rust
use autumn_web::prelude::*; // pagination_nav, PagerOptions, Page, …

#[get("/posts")]
async fn index(page_req: PageRequest, mut db: Db) -> AutumnResult<Markup> {
    let total: i64 = posts::table.count().get_result(&mut db).await?;
    let items: Vec<Post> = posts::table
        .limit(page_req.limit()).offset(page_req.offset())
        .select(Post::as_select())
        .load(&mut db).await?;
    let page = Page::new(items, total, &page_req);

    Ok(html! {
        ul { @for post in &page.content { li { (post.title) } } }
        // One line: an accessible, windowed pager below the list.
        (pagination_nav(&page, &PagerOptions::new("/posts")))
    })
}
```

The renderer emits a `<nav aria-label="Pagination">` containing previous/next
affordances and a **windowed** page-number sequence with first/last anchors and
ellipses (`1 … 4 5 6 … 20`). The active page carries `aria-current="page"`, and
disabled prev/next render as non-focusable `aria-disabled` spans.

### Preserving filters and sort

Pass the current request's query string to [`PagerOptions::query`] and the pager
keeps active filters, sort, and search on every link — swapping only the `page`
param:

```rust
// With ?q=foo&sort=name in the URL, every page link keeps q=foo&sort=name.
let opts = PagerOptions::new("/posts").query("q=foo&sort=name");
(pagination_nav(&page, &opts))
```

### htmx (opt-in)

By default the links are plain `<a href>` — pagination works with zero
JavaScript. Opt into htmx partial swaps with [`PagerOptions::hx_target`]:

```rust
let opts = PagerOptions::new("/posts")
    .hx_target("#post-list") // adds hx-get + hx-target to every link
    .hx_push_url();          // and updates the address bar
```

### Cursor feeds

For cursor pagination, [`cursor_pagination_nav`] renders prev/next affordances
from a `CursorPage` (there are no page numbers, since a cursor feed has no
total). The next link is built from `next_cursor`; supply
[`PagerOptions::prev_cursor`] for a back-link.

```rust
let opts = PagerOptions::new("/feed");
(cursor_pagination_nav(&cursor_page, &opts))
```

[`pagination_nav`]: https://docs.rs/autumn-web/latest/autumn_web/ui/pagination/fn.pagination_nav.html
[`cursor_pagination_nav`]: https://docs.rs/autumn-web/latest/autumn_web/ui/pagination/fn.cursor_pagination_nav.html
[`PagerOptions::query`]: https://docs.rs/autumn-web/latest/autumn_web/ui/pagination/struct.PagerOptions.html
[`PagerOptions::hx_target`]: https://docs.rs/autumn-web/latest/autumn_web/ui/pagination/struct.PagerOptions.html
[`PagerOptions::prev_cursor`]: https://docs.rs/autumn-web/latest/autumn_web/ui/pagination/struct.PagerOptions.html

---

## In the examples

| Example | Shows |
|---|---|
| `examples/reddit-clone` | offset pagination on a community listing (`/r/{slug}`): `PageRequest`, a filtered `COUNT`, `pagination_nav` below the list, a self-referential canonical on deeper pages, and page links that work with JavaScript disabled |
| `examples/todo-app` | `PageRequest` plus a hand-rolled prev/next pager over `Page<Todo>`, for when you want the markup |

---

## Further reading

- [`autumn_web::pagination`](https://docs.rs/autumn-web/latest/autumn_web/pagination/index.html) — API reference
- [Extractors guide](./extractors.md) — `PageRequest`, `CursorRequest` and
  `ListQuery` among the rest, and why their extraction is infallible
- [`#[repository]` macro](./macro-transparency.md) — generated method inventory
- [Generators guide](./generators.md) — `autumn generate scaffold` options
- [SEO guide](./seo.md) — canonical URLs on paginated pages
