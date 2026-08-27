# SEO — `seo(...)`, `sitemap.xml`, and `robots.txt`

A public web page needs three things before a search engine can index it well:

1. **Meta tags** in the `<head>` that give the page a title, a summary, and a
   preview card.
2. A **`sitemap.xml`** that lists every URL the crawler must visit.
3. A **`robots.txt`** that tells the crawler where it can go.

Autumn builds all three. You do three things:

- Declare the values that never change on the route attribute.
- Refine the values that change in the handler.
- Set the site base URL in `autumn.toml`.

The framework then mounts `/robots.txt` and `/sitemap.xml` for you.

> **Runnable example:** [`examples/reddit-clone`](../../examples/reddit-clone)
> wires the full feature. Its `src/seo.rs` holds the sitemap source and the
> canonical-URL helpers. Its `autumn.toml` holds the `[seo]` block. Its routes
> in `src/routes/` carry the `seo(...)` arguments.
> [`examples/blog`](../../examples/blog) shows the same feature on a
> locale-prefixed site.

---

## Turn the feature on

Set one value in `autumn.toml`:

```toml
[seo]
base_url = "https://example.com"
```

`base_url` does three jobs:

- It mounts `GET /robots.txt` and `GET /sitemap.xml`.
- It makes the absolute URLs that both documents need.
- It adds the `Sitemap:` line to `robots.txt`.

Set it to the real public host. A wrong `base_url` sends crawlers to URLs that
do not exist.

The routes also appear when you register a sitemap source (see
[Sitemap](#sitemapxml)). You do not need both.

---

## Meta tags on the route

Declare the values that never change on the route attribute. Add a `seo(...)`
argument, then take a `SeoMeta` parameter. The parameter arrives with the
declared values already in it.

```rust,ignore
use autumn_web::prelude::*;

#[get(
    "/about",
    seo(
        title = "About • Autumn Reddit",
        description = "Why this site exists and what it demonstrates.",
        og_type = "website"
    )
)]
pub async fn about(seo: SeoMeta) -> Markup {
    layout_with_seo(seo, html! { h1 { "About" } })
}
```

Two rules govern the argument:

- Each value must be a string literal. The macro reads it at compile time.
- An unknown key or a repeated key is a compile error.

The argument gives the handler **values, not markup**. The handler still
chooses where to write them. A route that declares `seo(...)` but never takes a
`SeoMeta` parameter renders nothing.

The extractor never fails. A route without a `seo(...)` argument gives the
handler an empty builder, so you can add the parameter to any route.

### Where the argument works

| Macro | Accepts `seo(...)` |
|---|---|
| `#[get]`, `#[post]`, `#[put]`, `#[patch]`, `#[delete]` | yes |
| `#[static_get]` | yes — the pre-rendered HTML carries the tags |
| `#[ws]` | no — a WebSocket upgrade serves no crawlable page |

### The keys

Each key matches one `SeoMeta` builder method.

| Key | Tag it renders |
|---|---|
| `title` | `<title>` |
| `description` | `<meta name="description">` |
| `canonical` | `<link rel="canonical">` |
| `robots` | `<meta name="robots">` |
| `og_title`, `og_description`, `og_image`, `og_type`, `og_url` | `<meta property="og:…">` |
| `twitter_card`, `twitter_title`, `twitter_description`, `twitter_image` | `<meta name="twitter:…">` |

`SeoMeta::render` writes only the tags you set. It also fills three gaps for
you:

- `og:title` falls back to `title`.
- `og:description` falls back to `description`.
- `og:url` falls back to `canonical`.

The Twitter title and description fall back in the same way, but only when you
set `twitter_card`. Set each value one time and let the fallbacks do the rest.

Match the card type to what you have. Use `summary` until the page can supply
an `og_image`. A `summary_large_image` card with no image renders as a plain
link.

---

## Refine the values in the handler

A detail page cannot put its title on the attribute, because the title comes
from the database. Declare the fixed part on the attribute. Add the changing
part in the handler.

```rust,ignore
#[get(
    "/r/{sub_slug}/posts/{post_slug}",
    seo(og_type = "article", twitter_card = "summary")
)]
pub async fn show(
    Path((sub_slug, post_slug)): Path<(String, String)>,
    seo: SeoMeta,
    mut db: Db,
) -> AutumnResult<Markup> {
    let post = load_post(&mut db, &sub_slug, &post_slug).await?;

    let seo = seo
        .title(format!("{} • Autumn Reddit", post.title))
        .description(summarize(&post.body, 155));

    Ok(layout_with_seo(seo, html! { h1 { (post.title) } }))
}
```

`SeoMeta` is a normal builder. Each method takes the builder and returns it, so
you can chain the calls.

---

## Render the tags in your layout

`SeoMeta::render` returns Maud `Markup`. Put one call in the `<head>` of your
layout. Every page then gets the same treatment.

```rust,ignore
pub fn layout_with_seo(seo: SeoMeta, content: Markup) -> Markup {
    html! {
        (PreEscaped("<!DOCTYPE html>"))
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                (seo.render())
                link rel="stylesheet" href="/static/css/app.css";
            }
            body { main { (content) } }
        }
    }
}
```

`render` writes the `<title>` tag, so the layout must not write its own. Give
each page a title, or the page gets no title at all.

Keep the old plain-title entry point if your application has many pages. Let it
build a `SeoMeta` and call the new function:

```rust,ignore
pub fn layout(title: &str, content: Markup) -> Markup {
    layout_with_seo(SeoMeta::new().title(format!("{title} — Autumn Reddit")), content)
}
```

`examples/reddit-clone` uses this pattern in `src/routes/layout.rs`. Most of its
pages still call `layout`. Only the pages that need more tags call
`layout_with_seo`.

---

## Canonical URLs

A canonical tag names the one true address of a page. Add it when the same
content answers at more than one URL. Three common causes are:

- a second route that redirects to the first, such as `/posts/{id}`;
- a tracking parameter in the query string;
- a page that appears under both `/` and `/index`.

The tag must hold an absolute URL, so the handler needs `base_url` at run time.
Read it one time at start-up and keep it in a `OnceLock`:

```rust,ignore
static BASE_URL: OnceLock<Option<String>> = OnceLock::new();

pub fn init_base_url(config: &AutumnConfig) {
    let base = config.seo.base_url.as_deref()
        .map(|url| url.trim_end_matches('/').to_owned());
    let _ = BASE_URL.set(base);
}

#[must_use]
pub fn with_canonical(seo: SeoMeta, path: &str) -> SeoMeta {
    match BASE_URL.get().and_then(Option::as_deref) {
        Some(base) => seo.canonical(format!("{base}{path}")),
        None => seo,
    }
}
```

Call `init_base_url` in `main` before the application starts. Then wrap the
builder in each handler:

```rust,ignore
let seo = with_canonical(seo.title(post.title.clone()), &post_path);
```

The `AutumnConfig` extractor also reads the value, but it clones the whole
configuration on each request. The `OnceLock` costs one pointer read.

---

## Keep a page out of the index

Set `robots` on the route:

```rust,ignore
#[secured]
#[get("/submit", seo(title = "Submit a post", robots = "noindex, nofollow"))]
pub async fn submit_form(seo: SeoMeta) -> AutumnResult<Markup> { /* … */ }
```

The directive has two effects:

1. The page renders `<meta name="robots" content="noindex, nofollow">`.
2. The framework drops the page from `sitemap.xml`, but only for a
   `#[static_get]` route. See the next rule for why.

Autumn reads `noindex` as a comma-separated directive, so `"noindex"`,
`"noindex, nofollow"`, and `"nofollow, noindex"` all count. An unrelated value
such as `"noarchive"` does not.

### The sitemap-exclusion rule

The framework filters only the paths it derives on its own. Those are the
concrete `#[static_get]` paths. It never filters the entries a sitemap source
supplies.

The reason is that a `SitemapEntry` carries only a URL string. Nothing ties the
entry back to a route, so the framework cannot match your URLs against your
route templates. It also must not drop a URL you asked for.

The practical result is one rule: **when you add a URL to a sitemap source, do
not also mark its route `noindex`.** Remove the URL from the source instead.

---

## `robots.txt`

The active profile sets the default:

| Profile | Body |
|---|---|
| `dev`, `test` | `User-agent: *` and `Disallow: /` |
| `prod`, `production` | `User-agent: *` and `Allow: /` |

Override the default and add your own rules under `[seo.robots]`:

```toml
[seo.robots]
# Force the answer, whatever the profile says. Use `false` on a staging host
# that runs the `prod` profile.
allow_all = false

# Extra lines. Autumn appends them after the User-agent block.
additional_rules = [
  "Disallow: /submit",
  "Disallow: /actuator/",
  "Crawl-delay: 5",
]

# An explicit Sitemap: URL. Autumn computes it from `base_url` when you omit it.
sitemap_url = "https://cdn.example.com/sitemap.xml"
```

`robots.txt` and a `noindex` tag do different jobs. Use both:

- `robots.txt` stops the **crawl**.
- `noindex` stops the **indexing**.

A page that another site links to can enter the index without a crawl. Only the
`noindex` tag keeps it out, and a crawler must fetch the page to read that tag.

---

## `sitemap.xml`

Autumn builds the document from two sources.

### 1. Static routes, derived for you

The framework adds one entry for each `#[static_get]` path when `base_url` is
set. It skips a path that holds a `{` placeholder, because a template is not a
URL.

### 2. A `SitemapSource` you register

Every other URL comes from your application. Implement `SitemapSource` and
register it on the builder:

```rust,ignore
use autumn_web::seo::{SitemapChangefreq, SitemapEntry, SitemapSource};
use std::future::Future;
use std::pin::Pin;

struct PostSitemapSource {
    pool: Option<Pool<RuntimeConnection>>,
}

impl SitemapSource for PostSitemapSource {
    fn entries(&self) -> Pin<Box<dyn Future<Output = Vec<SitemapEntry>> + Send + '_>> {
        Box::pin(async {
            vec![
                SitemapEntry::new("https://example.com/posts/hello")
                    .lastmod("2026-05-01")
                    .changefreq(SitemapChangefreq::Weekly)
                    .priority(0.6),
            ]
        })
    }
}

// In main():
autumn_web::app()
    .seo_source(PostSitemapSource { pool })
    .run()
    .await;
```

Each entry holds one URL and three optional hints:

| Field | Meaning |
|---|---|
| `loc` | The absolute URL. Autumn escapes the XML characters for you. |
| `lastmod` | The last change date, as `YYYY-MM-DD`. |
| `changefreq` | `Always`, `Hourly`, `Daily`, `Weekly`, `Monthly`, `Yearly`, or `Never`. |
| `priority` | A number from 0.0 to 1.0. Autumn clamps it. |

### Read the database in a source

The application state does not exist when the framework collects the entries,
so a source cannot use the `Db` extractor. Build a pool of its own from the
configuration:

```rust,ignore
let pool = autumn_web::db::create_pool(&config.database)?;
let source = PostSitemapSource { pool };
```

`examples/reddit-clone/src/seo.rs` does this. It lists the communities and the
posts.

Bound the query. It runs at boot, so a row count you did not choose must not
decide how long it takes. Log a warning when the bound bites, so a partial
sitemap is never a silent one.

### Give `lastmod` the page, not one column

`lastmod` tells a crawler whether it must fetch the page again. It must
therefore describe the whole page. One timestamp column rarely does.

A post page is its body plus its comment thread. An edit advances
`posts.updated_at`. A new comment does not: it writes a different table
through a different route. A `lastmod` read straight from `posts.updated_at`
therefore tells the crawler nothing changed, and the new comments stay out of
the index.

You have two ways to fix that:

1. **Write** the timestamp from each subsystem that changes the page.
2. **Derive** it in the sitemap query.

Prefer the second. One query then owns the definition, and no write path has
to remember to take part:

```sql
SELECT GREATEST(p.updated_at, COALESCE(MAX(c.created_at), p.updated_at)) AS last_modified
  FROM posts p
  LEFT JOIN comments c ON c.commentable_id = p.id AND c.deleted_at IS NULL
 GROUP BY p.id
 ORDER BY last_modified DESC
```

Order on the derived value, not on the column. Your entry cap cuts the list,
and it must cut the pages that really are the oldest.

Leave out the changes a reader does not come for. A vote count is one of them.
Search engines ask you not to advertise a trivial change. A sitemap that moves
every date on every vote also teaches crawlers to distrust all of its dates.

Handle a query failure inside the source. Log the failure and return the
entries you have. A short sitemap is better than a failed boot.

### The entries are a start-up snapshot

Autumn calls `entries()` one time, while it builds the router. It renders the
result into a fixed response body. Two things follow:

- The route costs nothing to serve. It writes one cached string.
- The sitemap does not change until the next deploy or restart.

That trade suits most sites, because a crawler reads the file hours after a
deploy. When your site must list new content at once, register your own route
instead:

```rust,ignore
#[get("/sitemap.xml")]
async fn sitemap(mut db: Db) -> impl IntoResponse {
    let entries = load_entries(&mut db).await;
    (
        [("Content-Type", "application/xml; charset=utf-8")],
        autumn_web::seo::sitemap_xml(&entries, Some("https://example.com")),
    )
}
```

Autumn sees the collision, logs a warning, and mounts neither of its own SEO
routes. Your route wins. Serve `/robots.txt` yourself as well in that case, or
build its body with `autumn_web::seo::robots_txt`.

### The 50,000-URL limit

The sitemap protocol allows 50,000 URLs in one file. `sitemap_xml` serves the
first 50,000 entries and logs a warning for the rest. A larger site needs a
sitemap index, which is a file that points at several sitemap files. Build the
index in your own `/sitemap.xml` route.

---

## Locale-prefixed sites

Turn on locale-prefixed routing in `autumn.toml`:

```toml
[i18n]
locale_prefix_enabled = true
supported_locales = ["en", "es"]
```

Autumn then writes one sitemap entry per locale for each derived static path.
`/about` becomes `https://example.com/en/about` and
`https://example.com/es/about`.

Tell the crawler which URLs are translations of each other with `hreflang`
links. `locale_alternates` builds the pairs, and `hreflang_alternates` renders
them:

```rust,ignore
use autumn_web::seo::{SeoMeta, locale_alternates};

#[get("/posts", seo(title = "Posts"))]
async fn index(locale: Locale, seo: SeoMeta) -> Markup {
    let seo = seo.hreflang_alternates(locale_alternates(
        "https://example.com",
        "/posts",
        "en",
        &["en".to_owned(), "es".to_owned()],
    ));
    html! { head { (seo.render()) } }
}
```

The function adds an `x-default` pair that points at the default locale. Pass
the locale-stripped path, which is what the `Uri` extractor gives you inside a
locale-prefixed nest.

Two exclusions apply, and both keep the sitemap in step with the router:

- Autumn excludes `#[static_get]` routes from locale prefixing, because
  pre-rendering requests one unprefixed path. Such a route stays one entry.
- Autumn lists a path from `[i18n] locale_prefix_exclude` without a prefix.

A `SitemapSource` entry is your own text. Autumn never rewrites it, so add the
locale prefix yourself. `examples/blog` shows both halves.

---

## Static builds

`autumn build` writes `robots.txt` and `sitemap.xml` into the output directory
next to the pre-rendered pages. It uses the same configuration and the same
sources as the server, so the two agree.

The build never overwrites a file your own routes already produced. A
`#[static_get("/robots.txt")]` route therefore wins, and the build prints a
line that says it skipped the file.

---

## Test it

Start the application and read the two documents:

```bash
curl http://localhost:3000/robots.txt
curl http://localhost:3000/sitemap.xml
curl -s http://localhost:3000/ | grep -E '<title>|og:|canonical'
```

In a test, assert on the rendered markup:

```rust,ignore
#[test]
fn post_page_declares_its_canonical_url() {
    let seo = SeoMeta::new()
        .title("hello • Autumn Reddit")
        .canonical("https://example.com/r/rust/posts/hello")
        .og_type("article");

    let html = layout_with_seo(seo, html! {}).into_string();

    assert!(html.contains(r#"<link rel="canonical" href="https://example.com/r/rust/posts/hello">"#));
    assert!(html.contains(r#"<meta property="og:type" content="article">"#));
}
```

`examples/reddit-clone` carries two suites:

- `src/routes/layout.rs` asserts the rendered tags. It needs no database.
- `tests/seo_pg_integration.rs` seeds a Postgres, runs the sitemap source, and
  asserts the URLs and the `lastmod` values. Run it with:

  ```bash
  cargo test -p reddit-clone --test seo_pg_integration -- --ignored
  ```

---

## Checklist

1. Set `[seo] base_url` to the real public host.
2. Put `(seo.render())` in the `<head>` of your layout.
3. Declare a `title` and a `description` on each public route.
4. Add a canonical URL on each page that answers at more than one address.
5. Mark each private page `robots = "noindex, nofollow"`.
6. Register a `SitemapSource` for the pages the framework cannot derive.
7. Read `/robots.txt` and `/sitemap.xml` before you deploy.

---

## Reference

| Item | Where |
|---|---|
| `seo(...)` route argument | `#[get]`, `#[post]`, `#[put]`, `#[patch]`, `#[delete]`, `#[static_get]` |
| `SeoMeta` builder and extractor | `autumn_web::seo::SeoMeta` |
| `locale_alternates` | `autumn_web::seo::locale_alternates` |
| `SitemapSource`, `SitemapEntry`, `SitemapChangefreq` | `autumn_web::seo` |
| `sitemap_xml`, `robots_txt` | `autumn_web::seo` |
| `AppBuilder::seo_source` | `autumn_web::app::AppBuilder` |
| `[seo]`, `[seo.robots]` | `autumn.toml` |

## Related guides

- [Internationalization](i18n.md) — locale-prefixed routing and the locale
  switcher that the `hreflang` links pair with.
- [Atom and RSS Feeds](feeds.md) — the other document a content site publishes
  for machines.
- [Getting Started](getting-started.md) — `#[static_get]`, the route kind whose
  paths Autumn puts in the sitemap for you.
- [Deployment](deployment.md) — profiles, which decide the `robots.txt`
  default.

---

*This guide follows [ASD-STE100](https://www.asd-ste100.org/) Simplified
Technical English: short sentences, active voice, simple tenses, and one
meaning per word. Keep that style when you edit it.*
