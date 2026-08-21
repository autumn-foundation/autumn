# Threaded Comments on Anything — `#[commentable]`

Comments are one of the most universal web jobs-to-be-done: blog posts,
articles, products, support tickets, photos. Yet the usual answer is a
`comments` table with a `post_id` column, a route that inserts into it, a
threading query, a recursive render, and a `comment_count` that nobody
remembers to decrement. Add comments to a *second* model and you copy the whole
thing — because the foreign key names one table.

`#[commentable]` is Autumn's answer, and it is a new **association kind**.
`belongs_to` / `has_many` / `has_one` / `through` all pin the child to exactly
one parent table. This one does not: a single `comments` table keyed on
`(commentable_type, commentable_id)` attaches to *any* number of parent models,
with a `parent_id` self-reference for threading.

```rust,ignore
#[autumn_web::model]
#[commentable(by = User, author_name = username)]
pub struct Post { /* … `pub comment_count: i64` … */ }

// The second model. This is the entire diff.
#[autumn_web::model]
#[commentable(by = User, author_name = username)]
pub struct Photo { /* … `pub comment_count: i64` … */ }
```

The attribute must be written **below** `#[model]` — attribute macros are
consumed top-down, so `#[commentable]` above `#[model]` is never seen by
anything and fails with `cannot find attribute commentable in this scope`.

> A complete runnable version lives in
> [`examples/reddit-clone`](../../examples/reddit-clone), where this feature
> replaced a 188-line hand-rolled `src/routes/comments.rs`. `Post` **and**
> `Subreddit` both carry `#[commentable]` in
> [`src/models.rs`](../../examples/reddit-clone/src/models.rs); the app mounts
> the framework's comment router once in
> [`src/main.rs`](../../examples/reddit-clone/src/main.rs);
> [`tests/commentable_pg_integration.rs`](../../examples/reddit-clone/tests/commentable_pg_integration.rs)
> exercises both against the example's real migrations. The framework's own
> [`autumn/tests/integration/commentable.rs`](../../autumn/tests/integration/commentable.rs)
> is the canonical evidence — the suite CI's ignored-test sweep runs on every push.

## Prerequisites

`#[commentable]` is part of `#[autumn_web::model]`, so it needs the default
Postgres/Diesel stack any Autumn app with a `#[model]` and a `#[repository]`
already has — no extra feature, no extra dependency. The widget needs `maud`
(also a default), and the generic router needs both. Both backends are
supported: `autumn generate scaffold … comments:commentable` emits the shared
table spelled for whichever one the project uses.

## What you get

| Surface | What it is |
|---|---|
| `Post::COMMENTABLE_TYPE` | the discriminator stored in `comments.commentable_type` |
| `Post::commentable_spec()` | the compile-time binding (table, columns, depth, counter) |
| `PostComments` trait | `add_comment` / `comment_thread` / `delete_comment` on the generated repository |
| `commentable::router(…)` | one pair of routes serving **every** commentable model |
| `widgets::comment_thread` | the no-JS/htmx nested list with an inline reply form per node |

## The table

One table, shared by every commentable model:

```sql
CREATE TABLE comments (
    id BIGSERIAL PRIMARY KEY,
    commentable_type TEXT NOT NULL,
    commentable_id BIGINT NOT NULL,
    parent_id BIGINT REFERENCES comments(id) ON DELETE CASCADE,
    author_id BIGINT NOT NULL,
    body TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT NOW(),
    deleted_at TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_comments_thread
    ON comments (commentable_type, commentable_id, created_at, id);
CREATE INDEX IF NOT EXISTS idx_comments_parent_id ON comments (parent_id);
```

`autumn generate scaffold post title:string comments:commentable` writes
exactly that, plus `comment_count BIGINT NOT NULL DEFAULT 0` on the scaffolded
model and the `#[commentable]` attribute. Run it again for a second model and
it adds only the column and the attribute — the table is shared, and the
generator will not recreate it — and if the project already has an unrelated
`comments` table, the migration fails loudly at `migrate` rather than no-opping
into a runtime `column "commentable_type" does not exist`.

### Why `commentable_id` has no foreign key

Because a single column cannot reference two tables. That is the known
trade-off of the polymorphic pattern, and Autumn does not paper over it: the
**write path is the referential check**. `add_comment` probes and row-locks the
parent row before it inserts, so an unknown (or soft-deleted) parent is a `404`
rather than a dangling comment. Nothing else in the module trusts
`commentable_id`.

The row lock is doing a second job too. It is held from the probe until commit,
so the counter `UPDATE` that follows can key on the parent id alone: no
concurrent writer can re-tenant or delete the row underneath it.

## The repository helpers

```rust,ignore
use crate::models::PostComments as _;   // the trait `#[commentable]` emits

// Post a top-level comment, or a reply.
let comment = posts.add_comment(post_id, author_id, "first!", None).await?;
let reply = posts.add_comment(post_id, author_id, "…", Some(comment.id)).await?;

// The whole live thread, replies nested under their parent.
let thread: Vec<CommentNode> = posts.comment_thread(post_id).await?;

// Delete a comment AND every reply beneath it. The record's id is part of the
// lookup, not decoration: without it any comment id would be deletable from any
// record of this model.
let removed: usize = posts.delete_comment(post_id, comment.id).await?;

// Rebuild the counter from the comments table, if it ever drifts.
let live: i64 = posts.recompute_comment_count(post_id).await?;
```

`comment_thread` is **one** query for the comments (plus the parent visibility
probe) whatever the nesting depth — the tree is assembled in Rust, never with
an N+1 walk. Order is stable: `(created_at, id)` at every level.

> **These helpers take their own pooled connection.** Like `#[votable]`'s
> `react()` and the many-to-many mutation helpers, they do not join an
> enclosing `Db::tx`. Never hold a `Db` extractor across one of these calls, or
> the handler needs two connections at once and deadlocks at pool-size
> concurrency — `drop(db)` first, as
> [`examples/reddit-clone`](../../examples/reddit-clone/src/routes/posts.rs)
> does.

`delete_comment` is idempotent. It soft-deletes the subtree (the comments table
carries `deleted_at`) and decrements the counter by the number of rows it
actually moved — so a double-submit removes nothing the second time and cannot
drive the count negative.

### `comment_count` is maintained, not computed

`posts.comment_count` moves with the same
[counter-cache](counter-cache.md) primitive `#[belongs_to(…, counter_cache)]`
uses: a single atomic `UPDATE posts SET comment_count = comment_count + $1`,
issued **inside the comment's own transaction**. A reader never sees a comment
whose count has not moved, or a count that moved without a comment. Opt out
with `counter_cache = false` on a model that keeps no count.

Counters drift anyway — imports, seeds, hand-written SQL — and they are
deliberately not clamped, so a drifted one can go negative rather than hide.
`recompute_comment_count(parent_id)` rebuilds it from the discriminator pair and
is idempotent. Do **not** reach for `counter_cache_recompute` instead: it keys
on the foreign-key column alone, and `commentable_id` is shared across models,
so it would count another model's comments that happen to share the id.

### Depth is bounded on the write path

`max_depth` (default `5`, where a top-level comment is depth `0`) is enforced
when the reply is posted, so the render never has to defend itself. A reply
that would nest deeper is `422`, as is a `reply_to` naming a comment on a
*different* record — without that check, anyone holding any comment id could
graft a subtree onto someone else's row.

## The widget

```rust,ignore
use autumn_web::widgets::{CommentThread, CommentView, comment_thread};

let thread = posts.comment_thread(post.id).await?;
use autumn_web::commentable::{CommentsConfig, thread_action, thread_dom_id};

let comments = CommentsConfig::default();          // must match what you `nest`
let cfg = CommentThread::from_spec(                 // depth + body caps from the model
        thread_dom_id(Post::COMMENTABLE_TYPE, post.id),
        thread_action(&comments, Post::COMMENTABLE_TYPE, post.id),
        Post::commentable_spec(),
    )
    .csrf_token(csrf.token())
    .return_to(&post_path);

Ok(comment_thread(&cfg, &CommentView::from_thread(&thread)))
```

It renders nested `<ol>`s (depth is exposed to assistive technology, not just
indented) with an inline reply form on every node, each inside a
`<details>`/`<summary>` disclosure. **No JavaScript is required for any of
it**: the reply forms are ordinary `POST` forms, and `return_to` brings the
browser back to the page it came from. When htmx *is* present, each form also
carries `hx-post` / `hx-target` / `hx-swap="outerHTML"`, so submitting a reply
replaces the whole thread region in place — no full page reload.

A signed-out visitor gets `.read_only().sign_in_prompt("Sign in to comment.")`:
the thread still renders, every form is gone. A rejected body comes back through
`.error(msg)`, rendered `role="alert"` above the form — htmx does not swap a
non-2xx response, so returning the `422` instead would make the button look
broken.

`from_spec` is the spelling to prefer: it copies `max_depth` **and**
`max_body_bytes` off the model, so the UI cannot drift from the write path and
offer a reply the server would refuse.

## The generic router — why the second model needs no routes

`#[commentable]` registers the model with a process-wide registry, and

```rust,ignore
AppBuilder::new()
    .nest("/comments", autumn_web::commentable::router(Default::default()))
```

mounts **one** pair of routes for the whole binary:

| Method | Path | Does |
|---|---|---|
| `GET` | `/comments/{commentable_type}/{parent_id}` | render the thread fragment |
| `POST` | `/comments/{commentable_type}/{parent_id}` | post a comment or reply, then re-render it |

`commentable_type` is matched against the registry, so an unregistered type is
a `404` — the router can only ever reach a model that really declared
`#[commentable]`. Adding a third commentable model changes nothing here.

**Mount it behind whatever authentication and CSRF middleware your app already
uses.** The `POST` handler reads the author's id from the session key named by
`CommentsConfig::session_author_key` (default `user_id`) and trusts it; it does
not itself authenticate, and it does not itself verify a CSRF token — Autumn's
CSRF layer does, and the widget renders the hidden field for it.

`mount_path` must match the path you pass to `nest`: it is what the rendered
form actions are built from, so a mismatch renders a perfect thread that `404`s
on every submit. Build your own page's thread with
[`commentable::thread_dom_id`] and [`commentable::thread_action`] rather than
hand-writing either — the router re-renders into that exact id after every
reply, and an id of your own devising would be replaced by the router's on the
first htmx swap, so every later swap would miss.

### Record-level authorization

The router authorizes the **tenant**, never the record. It dispatches on a
string, so it cannot know that `Ticket 42` is private — which means that on a
default `CommentsConfig`, `GET /comments/Ticket/42` reads any thread by id and
any signed-in user can comment on any record of any registered model.

For a forum or a blog, where the records are public anyway, that is exactly
right. **An app with private, draft, or role-gated records must set a hook:**

```rust,ignore
CommentsConfig::default().authorize(|access| Box::pin(async move {
    // access.commentable_type / .parent_id / .viewer_id / .write
    may_see(&access.commentable_type, access.parent_id, access.viewer_id).await
}))
```

A refusal is a `404`, not a `403` — a `403` would confirm the record exists.
An app that would rather not think about it can skip the router entirely: the
repository helpers and the widget work perfectly well from your own already
authorized handlers.

### Side effects on create

The router deliberately owns no app-specific behaviour, which would make
adopting it a downgrade for any app whose hand-rolled route did something on
create — sent a notification, pushed to a live feed, queued for moderation,
updated a search index. `on_comment` is where that goes back:

```rust
CommentsConfig::default().on_comment(|created| Box::pin(async move {
    // `created` carries commentable_type, parent_id, comment_id, reply_to,
    // author_id and the body as accepted — no read-back required.
    my_app::live_feed::announce(&created).await;
}))
```

It runs **after** the comment's transaction commits, so the row is durable and
visible on other connections. A failing callback is logged and the request
still succeeds: a broken notifier must not un-post a comment the user can
already see. A *rejected* comment (blank body, over the cap, past `max_depth`)
never reaches it, because no row was created.

`examples/reddit-clone` uses exactly this to keep `/ws/feed` and
`/ws/r/{slug}` announcing new comments after moving to the generic router.

## Options

Every key is optional except that `author_name` needs somewhere to read the
name from.

```rust,ignore
#[commentable(
    by = User,                    // the author model; also supplies `author_table`
    author_name = username,       // display-name column; omitted → `user #id`
    author_table = users,         // override the table derived from `by`
    author_pk = id,
    type_name = "Post",           // discriminator; defaults to the Rust type name
    table = comments,
    counter_cache = comment_count, // or `false` to keep no counter
    max_depth = 5,
    max_body = 10000,             // bytes
    soft_delete = true,
    // column overrides, for an existing schema
    comment_pk = id, type_column = commentable_type, id_column = commentable_id,
    parent_column = parent_id, author_column = author_id, body_column = body,
    created_at_column = created_at,
)]
```

Two of these are worth a second look:

- **`type_name`** defaults to the model's Rust type name, so *renaming the
  struct changes the discriminator* and orphans existing comment rows. Pin it
  before renaming a model that already has comments in production.
- **`author_name`** is deliberately unset by default. The framework will not
  guess a column, and a scaffolded `User` carries an `email` — defaulting a
  *public* display name to it would leak addresses into every rendered thread.

## Multi-tenancy

A model with a `tenant_id` column gets the same treatment `#[votable]` does:
through a `#[repository(…, tenant_scoped)]` repository, the parent is matched on
`tenant_id` too, so another tenant's `parent_id` is `NotFound` before anything
is written or read. `across_tenants()` opts out; a `tenant_scoped` repository
with no tenant context is an error. A model without the column emits no tenant
predicate at all.

## What this deliberately does not do

- **Voting on comments** — composes with [`#[votable]`](votable.md).
- **Rich-text bodies** — composes with the safe rich-text field (#1255).
  Bodies are plain text and are HTML-escaped on render.
- **Live cross-client updates** — composes with model-change broadcast (#1336).
- **Moderation queues, spam scoring, edit history, @mentions, email-on-reply** —
  follow-ups.
- **Paginate a thread.** `comment_thread` loads every live comment for the
  record in one query. That is the right shape for the discussion sizes this
  targets and is what makes "one query at any depth" true; a record with a
  million comments wants a different API, not a `LIMIT` bolted onto this one.
- **Nest without bound.** `max_depth` must stay below `1000`, the depth the
  runtime's recursive probe measures to; beyond that the render flattens rather
  than recursing, so a malformed chain cannot overflow the stack.

## See also

- [Counter Caches](counter-cache.md) — the mechanism `comment_count` is
  maintained by.
- [Votes, Likes and Reactions](votable.md) — the sibling association kind, and
  what comment voting composes with.
- [Generators](generators.md#threaded-comments-on-anything-with-commentable) —
  the `comments:commentable` scaffold token.
- [ADR-0008](../adr/0008-associations-and-eager-loading.md) — the association
  design this extends.
