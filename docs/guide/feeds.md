# Atom and RSS Feeds

A content site — a blog, a changelog, a podcast, a news section — wants a
syndication feed so readers can subscribe in a feed reader and pollers can pick
up new posts without scraping HTML. Autumn's `feed` module builds a well-formed
[Atom 1.0](https://datatracker.ietf.org/doc/html/rfc4287) or
[RSS 2.0](https://www.rssboard.org/rss-specification) document from channel
metadata plus a list of entries, and returns it straight from a `#[get]`
handler with the correct `Content-Type` and every text field XML-escaped.

A `Feed` is a plain builder: you name the channel, push `FeedEntry` items, and
either return the `Feed` directly (it implements `IntoResponse`) or wrap it in
`Feed::conditional` so feed pollers get a `304 Not Modified` when nothing has
changed. There is no plugin to install, no config surface, and no table — it is
pure rendering over data you already have.

For a runnable end-to-end route, see the
[`examples/blog`](../../examples/blog) crate, whose
`src/routes/feed.rs` serves an Atom feed of published posts at `/feed.xml`.

## Prerequisites

The feed types live in `autumn_web::feed` (they are not in the prelude):

```rust,ignore
use autumn_web::feed::{Feed, FeedEntry};
use autumn_web::prelude::*;
```

Timestamps are `chrono::DateTime<Utc>`, so a handler that dates its entries also
pulls in `chrono`:

```toml
[dependencies]
autumn-web = "0.6"
chrono = "0.4"
```

## Building a feed

Start a feed with `Feed::atom` or `Feed::rss`. Both take the same three pieces of
channel metadata:

- **title** — the feed's display name.
- **site_link** — the human-facing site URL (the Atom `alternate` link / RSS
  `<link>`).
- **self_link** — the canonical URL of the feed document itself (the Atom `self`
  link / RSS `atom:link rel="self"`). It should match the route the feed is
  served from.

Append items with `entry` (one) or `entries` (an iterator), and set optional
channel fields with the chainable builders:

```rust,ignore
use autumn_web::feed::{Feed, FeedEntry};
use chrono::{TimeZone, Utc};

let published = Utc.with_ymd_and_hms(2026, 5, 31, 12, 0, 0).unwrap();

let feed = Feed::atom(
    "My Blog",
    "https://example.com/",
    "https://example.com/feed.xml",
)
.author("Jane Doe")
.description("Notes on Rust and the web")
.entries([
    FeedEntry::new(
        "https://example.com/posts/hello", // stable, globally-unique id
        "Hello, world",                    // entry title
        "https://example.com/posts/hello", // link to the item
    )
    .summary("First post")
    .published(published)
    .updated(published),
]);

let xml = feed.render();
assert!(xml.contains("<feed xmlns=\"http://www.w3.org/2005/Atom\">"));
```

### Channel builders

| Method | Effect |
|--------|--------|
| `.author(name)` | Feed author. Atom uses it for `<author><name>`; RSS emits `<dc:creator>`. |
| `.description(text)` | Channel description. Required by RSS — it defaults to the title when unset. |
| `.updated(dt)` | Channel `updated` / `lastBuildDate`. When unset it is derived from the newest entry. |
| `.entry(entry)` | Append one `FeedEntry`. |
| `.entries(iter)` | Append every `FeedEntry` an iterator yields. |

### Entry builders

A `FeedEntry` starts from its stable `id`, `title`, and `link`. The `id` should
be a stable, globally-unique identifier — a permalink URL is the usual choice.
The rest are optional:

| Method | Effect |
|--------|--------|
| `.summary(text)` | Short summary (Atom `<summary>`, RSS `<description>`). |
| `.content(html)` | Full body (Atom `<content type="html">`, preferred RSS body). |
| `.published(dt)` | Publication timestamp. |
| `.updated(dt)` | Last-updated timestamp. |

When an entry sets both `content` and `summary`, RSS prefers `content` for its
`<description>`; Atom emits both. When only some entries are dated, the feed's
`<updated>` / `lastBuildDate` falls back to the newest dated entry, and finally
to a deterministic epoch timestamp so the rendered bytes stay stable across
renders.

## Serving a feed from a handler

`Feed` implements `IntoResponse`, so a handler can return it directly. The
response carries the format's `Content-Type` (`application/atom+xml` for Atom,
`application/rss+xml` for RSS), UTF-8, with every field XML-escaped:

```rust,ignore
use autumn_web::feed::{Feed, FeedEntry};
use autumn_web::prelude::*;

const SITE: &str = "https://example.com";

#[get("/feed.xml")]
async fn feed_xml(mut db: Db) -> AutumnResult<impl IntoResponse> {
    let posts = Post::published(&mut db).await?;

    let feed = Feed::atom("My Blog", SITE, format!("{SITE}/feed.xml"))
        .author("My Blog")
        .entries(posts.iter().map(|p| {
            let url = format!("{SITE}/posts/{}", p.slug);
            FeedEntry::new(url.clone(), p.title.as_str(), url)
                .summary(p.body.as_str())
                .published(p.created_at.and_utc())
                .updated(p.updated_at.and_utc())
        }));

    Ok(feed)
}
```

Because the entries come straight from an iterator over your rows, the feed is
always a projection of live data — there is no separate feed store to keep in
sync.

## Atom vs RSS

The two formats share the exact same builder; only the constructor differs
(`Feed::atom` vs `Feed::rss`) and, with it, the rendered document and
`Content-Type`:

- **Atom 1.0** (`Feed::atom`, `application/atom+xml`) is the newer, stricter
  format. Atom requires a feed-level author unless every entry carries its own,
  so when you do not call `.author(...)` the feed title is used as the author to
  keep the document valid. Timestamps render as RFC 3339.
- **RSS 2.0** (`Feed::rss`, `application/rss+xml`) is the widely-supported
  classic format, and the one most podcast and news tooling expects. It requires
  a channel `<description>`, which defaults to the title when unset, and renders
  timestamps as RFC 2822.

If you have no specific compatibility requirement, Atom is the modern default;
choose RSS when a consumer (a podcast host, a legacy aggregator) asks for it. You
can even serve both from two routes over the same data — the builder is identical
and cheap to re-render.

Inspect the negotiated `Content-Type` without rendering via
`feed.content_type()`, which returns the header value for the feed's format.

## Conditional GET (304 for pollers)

Feed readers poll — often every few minutes, forever. Re-transmitting an
unchanged feed on every poll is wasted bandwidth. `Feed::conditional` wraps the
feed in HTTP conditional-GET handling, reusing Autumn's
[`etag`](conditional-get.md) machinery: it computes a weak `ETag` over the exact
rendered bytes and answers `304 Not Modified` when the client's `If-None-Match`
matches, skipping the body entirely.

```rust,ignore
use autumn_web::feed::{Feed, FeedEntry};
use autumn_web::reexports::axum::response::Response;
use autumn_web::reexports::http::HeaderMap;
use autumn_web::prelude::*;

const SITE: &str = "https://example.com";

#[get("/feed.xml")]
async fn feed_xml(mut db: Db, headers: HeaderMap) -> AutumnResult<Response> {
    let posts = Post::published(&mut db).await?;

    let feed = Feed::atom("My Blog", SITE, format!("{SITE}/feed.xml"))
        .author("My Blog")
        .entries(posts.iter().map(|p| {
            let url = format!("{SITE}/posts/{}", p.slug);
            FeedEntry::new(url.clone(), p.title.as_str(), url)
                .summary(p.body.as_str())
                .published(p.created_at.and_utc())
                .updated(p.updated_at.and_utc())
        }));

    Ok(feed.conditional(&headers))
}
```

The `ETag` is **weak** (a weak hash of the body) so it stays a valid validator
even when the framework applies a content-coding such as gzip or brotli to the
response. It changes whenever any rendered field changes — including two edits
within the same whole second, or a content edit that leaves timestamps untouched
— and is stable otherwise. A `Last-Modified` header is emitted for caches and
readers, but a coarse whole-second `If-Modified-Since` is deliberately not
honored on its own, so a body that changes within the same second can never be
served stale. If you only want the validator, `feed.etag()` returns the weak
`ETag`, and `feed.last_updated()` returns the newest timestamp across the feed.

## Try it in the blog example

The [`examples/blog`](../../examples/blog) crate serves a real feed at
`/feed.xml` from `src/routes/feed.rs`: it builds an Atom feed of the blog's
published posts and returns `feed.conditional(&headers)` for `304` support. With
the example running:

```bash
curl -i localhost:3000/feed.xml
```

The first request returns `200 OK` with `Content-Type: application/atom+xml` and
the feed body plus an `ETag`. Repeat the request with that tag and you get an
empty `304`:

```bash
curl -i -H 'If-None-Match: "<etag-from-first-response>"' localhost:3000/feed.xml
```

## See also

- [Conditional GET and ETags](conditional-get.md) — the `etag` layer
  `Feed::conditional` builds on.
- [Compression](compression.md) — why the feed `ETag` is weak (safe across
  content-codings).
