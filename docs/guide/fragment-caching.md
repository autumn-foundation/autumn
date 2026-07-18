# Maud fragment caching

Autumn can cache a **rendered Maud fragment** keyed by a record's identity and
version, so unchanged rows are served from cache and re-render automatically the
moment the record is written — without you hand-rolling cache keys or
invalidation.

This is the view-layer companion to Autumn's other caching primitives:

| Primitive | Caches | Granularity |
|-----------|--------|-------------|
| `#[cached]` | repository / function results | data |
| `CacheResponseLayer` | whole HTTP responses | per-URL |
| **`cache_fragment`** | **rendered `Markup`** | **per-fragment** |

Whole-response caching is too coarse — any per-user chrome (a CSRF token, a
"signed in as…" banner) busts the entire page. Data caching skips the query but
still re-runs the `html!{}` work. `cache_fragment` sits in between: it caches the
expensive render of an individual row, card, or feed item.

## Why it matters

For Autumn's Maud + htmx hybrid-rendering model, server-side render time *is* the
latency budget. A list or feed view re-renders dozens of identical partials per
request. Caching each fragment by `(identity, version)` means:

- a warm list of unchanged rows skips the render entirely, and
- a write to any record produces a freshly rendered fragment on the *very next*
  request — zero stale renders, no manual eviction.

## Quick start

```rust
use autumn_web::prelude::*;

fn post_card(post: &Post) -> Markup {
    cache_fragment_global(
        // identity: which fragment is this?
        format_args!("post_card:{}", post.id),
        // version: bump this whenever the record changes. Use a sub-second
        // resolution token (micros) so two edits in the same second still
        // produce distinct keys.
        post.updated_at.and_utc().timestamp_micros(),
        // optional TTL (None = no expiry)
        None,
        // render closure: only runs on a cache miss
        || html! {
            article {
                h2 { (post.title) }
                p { (post.body) }
            }
        },
    )
}
```

On the first request the closure runs and the markup is stored. On every
subsequent request — until `post.updated_at` changes — the cached markup is
returned and the closure is **not** executed. Editing the post bumps
`updated_at`, which changes the key, and the card re-renders once.

Use it in a list view exactly as you would any Maud component:

```rust
#[get("/")]
pub async fn index(mut db: Db) -> AutumnResult<Markup> {
    let posts = Post::published(&mut db).await?;
    Ok(html! {
        div class="space-y-4" {
            @for post in &posts {
                (post_card(post))   // each row cached independently
            }
        }
    })
}
```

## The two helpers

```rust
use autumn_web::cache::{cache_fragment, cache_fragment_global};
```

### `cache_fragment_global`

Resolves the **process-global** cache backend — the one registered with
[`AppBuilder::with_cache_backend`](#wiring-a-backend) or `AppState::set_cache`.
This is the ergonomic choice inside component functions that don't hold a cache
handle.

### `cache_fragment`

Takes an explicit `Option<&dyn Cache>`. Use it when you already have the cache in
hand — for example from `AppState::cache()` in a handler:

```rust
#[get("/")]
pub async fn index(state: AppState, mut db: Db) -> AutumnResult<Markup> {
    let cache = state.cache();           // Option<Arc<dyn Cache>>
    let posts = Post::published(&mut db).await?;
    Ok(html! {
        @for post in &posts {
            (cache_fragment(
                cache.as_deref(),
                format_args!("post_card:{}", post.id),
                post.updated_at.and_utc().timestamp_micros(),
                None,
                || render_post_card(post),
            ))
        }
    })
}
```

## Choosing the identity and version

The cache key is built from both halves you supply. The identity is
length-prefixed internally, so a `:` (or any other character) inside it can
never shift the identity/version boundary or alias two distinct fragments —
`(identity="a:b", version="c")` and `(identity="a", version="b:c")` stay
separate. You can put whatever you like in either half.

- **identity** — anything `Display` that uniquely names the fragment. Include the
  record type and primary key (`format_args!("post_card:{}", post.id)`), and any
  variant that changes the markup (locale, compact-vs-card layout, viewer role):
  `format_args!("post_card:{}:{}", post.id, locale.tag())`.
- **version** — a token that changes whenever the record changes. The natural
  choices are the record's `updated_at` timestamp or a version-history sequence
  number (see [version history](version-history.md)). Bumping it yields a cache
  miss, so a write naturally re-renders the fragment. Prefer a **sub-second**
  token — `timestamp_micros()` rather than `timestamp()` — so two writes within
  the same wall-clock second still produce distinct keys (a sequence number
  sidesteps this entirely).

> **Important:** if the fragment's appearance depends on data *outside* the
> record (e.g. the current user's vote state), fold that into the identity or
> version, or don't cache it — otherwise stale variants can be served.

## Russian-doll nesting

Fragments compose. An outer fragment whose render closure calls
`cache_fragment` for its children reuses the children's cached markup:

```rust
fn comment_thread(thread: &Thread) -> Markup {
    cache_fragment_global(
        format_args!("thread:{}", thread.id),
        // outer version derived from the children, so the outer re-renders
        // whenever *any* child changes
        thread.comments.iter().map(|c| c.updated_at).max()
            .map(|t| t.and_utc().timestamp_micros())
            .unwrap_or(0),
        None,
        || html! {
            ul {
                @for comment in &thread.comments {
                    (comment_card(comment))   // each inner fragment cached too
                }
            }
        },
    )
}

fn comment_card(comment: &Comment) -> Markup {
    cache_fragment_global(
        format_args!("comment:{}", comment.id),
        comment.updated_at.and_utc().timestamp_micros(),
        None,
        || html! { li { (comment.body) } },
    )
}
```

When one comment is edited:

1. that comment's `updated_at` bumps → its inner fragment re-renders,
2. the thread's derived version bumps → the outer fragment re-renders,
3. but the **unchanged sibling** comments are served straight from cache — their
   render closures never run.

Deriving the outer version from `max(child.updated_at)` is what wires the
invalidation together; Autumn does not track cross-fragment dependencies for you.

## TTL

The fourth argument is an optional `Duration`. It is forwarded to backends that
support native expiry (e.g. Redis `PSETEX`):

```rust
cache_fragment_global(
    format_args!("trending:{}", id),
    version,
    Some(std::time::Duration::from_secs(300)),  // expire after 5 min
    || html! { /* … */ },
);
```

The in-process moka backend manages TTL at the store level (configured when the
`MokaCache` is built), so the per-call TTL is most useful with Redis.

## Wiring a backend

Fragment caching uses the same pluggable [`Cache`] store as the rest of Autumn.
Register one when building the app:

```rust
autumn_web::app()
    // in-process, single node:
    .with_cache_backend(autumn_web::cache::MokaCache::new(1_000, None))
    // …or the Redis plugin for a cache shared across replicas.
    .run()
    .await;
```

Because rendered fragments are stored as their HTML `String`, they round-trip
through both the moka in-process store and the Redis shared store transparently.

## Graceful fallback

If **no** cache backend is configured — common in local dev without Redis or
moka — `cache_fragment_global` (and `cache_fragment(None, …)`) render the
fragment directly and never panic. Caching is a transparent optimization: code
written against these helpers works identically with or without a cache.

## See also

- [`#[cached]` and data caching](cloud-native.md) — cache query/function results
- [Conditional GET and ETags](conditional-get.md) — skip the *network* path
- [Version history](version-history.md) — per-record version tokens
- [Cache Stampede Protection](cache-stampede.md) — coalesce concurrent misses
  into a single fill
