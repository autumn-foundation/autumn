# Autumn Blog Example

A blog engine built with Autumn's full-stack path: Diesel, Maud, Tailwind,
htmx, embedded migrations, and the new hybrid-rendering pipeline.

## What it demonstrates

- **Encrypted credentials** — `config/credentials/development.toml.enc` is
  committed to the repo; the master key (`config/master.key`) is excluded by
  `.gitignore`. Visit `/api/credentials-status` after setting the key.
- Public blog listing and slug-based post pages
- Admin UI for create, edit, publish, and delete
- First-party `autumn-admin-plugin` mounted at `/backoffice`
- Built-in jobs dashboard for the demo `publish_webhook` job
- JSON endpoints alongside server-rendered HTML
- Validation before insert/update
- `#[static_get]` + `static_routes![]` for build-time rendering
- Automatic migrations on startup
- Framework health and actuator endpoints
- **i18n end-to-end** — opt-in `i18n` feature, `i18n/{en,es}.ftl`
  translations, the request-scoped `Locale` extractor, the `t!()` macro
  with **compile-time key validation**, automatic fallback, and a
  locale switcher in the layout (visit `/greet`)

## Prerequisites

- Rust 1.88.0+
- PostgreSQL (via Docker Compose below)
- Tailwind CSS binary (downloaded by `autumn setup`)

## Quick start

From the workspace root:

```bash
# 1. Download Tailwind CSS
cargo run -p autumn-cli -- setup

# 2. Start Postgres
docker compose -f examples/blog/docker-compose.yml up -d

# 3. Run the app (migrations are embedded and applied on startup)
cargo run -p blog
```

Open <http://localhost:3000>.

## Encrypted credentials

The blog example ships a pre-encrypted `config/credentials/development.toml.enc`
with placeholder API keys.  To see the credentials workflow end-to-end:

```bash
# The example master key (never use this in production):
export AUTUMN_MASTER_KEY=a0b1c2d3e4f5061728394a5b6c7d8e9f0a1b2c3d4e5f6071829304a5b6c7d8ef

# Or load from the file Autumn generated during `autumn new`:
# export AUTUMN_MASTER_KEY=$(cat config/master.key)

# Run the app — credentials are decrypted at boot from config/credentials/development.toml.enc
cargo run -p blog

# Verify credentials are loaded
curl http://localhost:3000/api/credentials-status
# → {"credentials_loaded":true,"sendgrid_configured":false,"stripe_configured":false}

# Edit credentials (opens $EDITOR):
autumn credentials edit

# Show redacted summary:
autumn credentials show

# Show full decrypted content:
autumn credentials show --reveal
```

The master key file (`config/master.key`) is in `.gitignore` — it is never
committed.  The encrypted file (`config/credentials/development.toml.enc`) is
safe to commit; without the master key it is indistinguishable from random bytes.

See [`docs/guide/credentials.md`](../../docs/guide/credentials.md) for the full
reference, file format, and production deployment guidance.

## Generated admin adapter

The blog's `PostAdmin` in `src/admin.rs` was built by hand to show the full
adapter contract. You can regenerate an equivalent adapter with:

```bash
autumn generate admin Post \
  title:String \
  slug:String \
  body:Text \
  published:bool \
  created_at:DateTime \
  updated_at:DateTime \
  --readonly slug \
  --readonly created_at \
  --readonly updated_at \
  --dry-run
```

Remove `--dry-run` to write the file (use `--force` to overwrite the existing
`src/admin.rs`). The generated adapter supports list with search and filters,
detail, create, update, delete, and bulk-delete — all wired through the
`autumn-admin-plugin` CRUD UI at `/backoffice/posts`.

To smoke-test the generated admin resource against a running blog server:

```bash
AUTUMN_TEST_BASE_URL=http://localhost:3000 \
AUTUMN_TEST_ADMIN_SESSION=<session_cookie> \
cargo test -p blog post_admin
```

See [`docs/guide/admin.md`](../../docs/guide/admin.md) for the full
walkthrough, customisation flags, and security notes.

## Try the jobs dashboard

The blog registers a demo `publish_webhook` background job and mounts the
first-party admin plugin at `/backoffice` with role checks disabled for the
example. Queue a job, then inspect the dashboard:

```bash
curl -X POST http://localhost:3000/api/posts/1/enqueue-publish-webhook
curl http://localhost:3000/backoffice/jobs
```

The jobs page shows enqueued, running, completed, and failed work, plus the
registered scheduled cleanup task.

## Subscribe to the feed

The blog exposes an Atom 1.0 feed of published posts at `/feed.xml`, built with
Autumn's `feed::Feed` renderer. The whole route is under 20 lines of wiring
(see [`src/routes/feed.rs`](src/routes/feed.rs)):

```rust
use autumn_web::feed::{Feed, FeedEntry};
use autumn_web::reexports::axum::response::Response;
use autumn_web::reexports::http::HeaderMap;
use autumn_web::{AutumnResult, Db, get};

use crate::models::Post;

const SITE_URL: &str = "https://autumn-demo.example.com";

#[get("/feed.xml")]
pub async fn feed_xml(mut db: Db, headers: HeaderMap) -> AutumnResult<Response> {
    let posts = Post::published(&mut db).await?;
    let feed = Feed::atom("Autumn Blog", SITE_URL, format!("{SITE_URL}/feed.xml"))
        .author("Autumn Blog")
        .entries(posts.iter().map(|p| {
            let url = format!("{SITE_URL}/posts/{}", p.slug);
            FeedEntry::new(url.clone(), p.title.as_str(), url)
                .summary(p.body.as_str())
                .published(p.created_at.and_utc())
                .updated(p.updated_at.and_utc())
        }));
    Ok(feed.conditional(&headers))
}
```

`Feed` implements `IntoResponse` and sets `Content-Type: application/atom+xml`,
XML-escaping every text field. `Feed::conditional(&headers)` reuses Autumn's
`etag` layer, so an unchanged feed is served as `304 Not Modified`:

```bash
# Fetch the feed and note the ETag header
curl -i http://localhost:3000/feed.xml

# Repeat with the ETag — the server responds 304 Not Modified with no body
curl -i -H 'If-None-Match: "<etag-from-above>"' http://localhost:3000/feed.xml
```

Swap `Feed::atom` for `Feed::rss` (same arguments) to serve RSS 2.0 with
`Content-Type: application/rss+xml` instead.

## Try the hybrid-rendering flow

The `/about` page is declared with `#[static_get("/about")]`.

```bash
# Pre-render static routes into dist/
cargo run -p autumn-cli -- build -p blog
```

Dynamic routes like `/` and `/posts/{slug}` still render through the server;
static routes get written to `dist/` for deployment or CDN serving.

## Try the i18n flow

The blog enables the `i18n` Cargo feature on `autumn-web` and ships
two translation bundles at `i18n/en.ftl` and `i18n/es.ftl`. The shared
`layout()` is fully translated through the `t!()` macro, the nav has
a built-in language switcher, and `/greet` renders an end-to-end demo:

```text
http://localhost:3000/greet              # default (Accept-Language)
http://localhost:3000/greet?locale=es    # Spanish via query override
http://localhost:3000/greet?locale=en    # English via query override
```

Resolution order is documented in [`docs/guide/i18n.md`](../../docs/guide/i18n.md):
query → signed session cookie → plain `autumn_locale` cookie →
`Accept-Language` → default. Try editing a key in `i18n/en.ftl` or
introducing a typo in a `t!()` call — the build fails with a
"did you mean" hint thanks to the proc-macro's compile-time check.

## Fingerprinted assets (production cache-busting)

Autumn ships a zero-config asset fingerprinting pipeline.  When you run a
release build the CLI hashes every file under `static/`, writes a
content-hashed copy alongside the original, and records the mapping in
`static/.autumn-manifest.json`:

```
static/css/autumn.css            ← original (kept for dev)
static/css/autumn.a1b2c3d4.css  ← fingerprinted copy (served in production)
static/.autumn-manifest.json    ← logical → fingerprinted path map
```

### End-to-end production deploy story

```bash
# 1. Build a release binary and fingerprint all static assets in one step
cargo run -p autumn-cli -- build -p blog

# 2. Start the server (reads static/.autumn-manifest.json at runtime)
./target/release/blog
```

Templates call `asset_url("css/autumn.css")` instead of a hard-coded path:

```rust
// In src/routes/posts.rs — unchanged across environments
link rel="stylesheet" href=(asset_url("css/autumn.css"));
```

| Build mode | Resolved URL |
|-----------|--------------|
| `cargo run` (debug) | `/static/css/autumn.css` |
| `cargo build --release` | `/static/css/autumn.a1b2c3d4.css` |

The fingerprinted URL is served with `Cache-Control: public, max-age=31536000,
immutable`, so browsers cache it forever.  On the next deploy the hash changes
automatically — the old URL is gone, the browser fetches the new one, and no
CDN or cache purge is needed.

Non-fingerprinted static paths (`/static/css/autumn.css`) are served with
`Cache-Control: public, max-age=0, must-revalidate` so development and fallback
paths always stay fresh.

### How `asset_url` works

```rust
use autumn_web::prelude::*;   // asset_url is in the prelude

// debug build  → "/static/css/autumn.css"
// release build → "/static/css/autumn.a1b2c3d4.css"
let url = asset_url("css/autumn.css");
```

If `static/.autumn-manifest.json` is absent (e.g. on a dev machine that never
ran `autumn build --release`), `asset_url` falls back to the plain
`/static/...` URL — so the app keeps running without any manual configuration.

## Inspecting routes with `autumn routes`

Print every mounted route without starting the server:

```bash
cargo run -p autumn-cli -- routes -p blog
```

Example output:

```
METHOD    PATH                        HANDLER                  SOURCE
DELETE    /admin/{id}                 delete_post              user
GET       /                           list_posts               user
GET       /about                      about                    user
GET       /actuator/health            actuator                 framework
GET       /actuator/info              actuator                 framework
GET       /actuator/metrics           actuator                 framework
GET       /admin                      admin_posts              user
GET       /admin/new                  new_post_form            user
GET       /admin/{id}/edit            edit_post_form           user
GET       /api/posts                  api_list_posts           user
GET       /backoffice/posts           backoffice_list_posts    plugin:autumn-admin
GET       /health                     health                   framework
GET       /posts/{slug}               show_post                user
POST      /admin                      create_post              user
POST      /api/posts                  api_create_post          user
```

Filter to just the JSON API routes and emit machine-readable JSON:

```bash
cargo run -p autumn-cli -- routes -p blog /api --format json
```

Hide framework-internal routes to review only application routes:

```bash
cargo run -p autumn-cli -- routes -p blog --user-only
```

Capture a snapshot for `git diff` auditing:

```bash
cargo run -p autumn-cli -- routes -p blog > routes.txt
git diff routes.txt
```

## Routes

### HTML

| Method | Path | Description |
|--------|------|-------------|
| GET | `/` | Public blog listing |
| GET | `/about` | Static page rendered via `#[static_get]` |
| GET | `/greet` | i18n demo with locale switcher |
| GET | `/feed.xml` | Atom feed of published posts (conditional GET / 304) |
| GET | `/posts/{slug}` | View a published post |
| GET | `/admin` | Admin post dashboard |
| GET | `/admin/new` | New post form |
| POST | `/admin` | Create a post |
| GET | `/admin/{id}/edit` | Edit post form |
| POST | `/admin/{id}` | Update a post |
| DELETE | `/admin/{id}` | Delete a post with htmx |
| GET | `/backoffice/jobs` | Admin plugin jobs dashboard |
| GET | `/backoffice/posts` | Admin plugin list view for posts |

### JSON API

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/posts` | List published posts as JSON |
| POST | `/api/posts` | Create a post as JSON |

### Framework

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health probe |
| GET | `/actuator/health` | Detailed health view |
| GET | `/actuator/info` | Build and runtime metadata |
| GET | `/actuator/metrics` | Request and pool metrics |
| GET | `/static/js/htmx.min.js` | Bundled htmx (plain) or `/static/js/htmx.min.<hash>.js` (release) |
| GET | `/static/css/autumn.css` | Compiled Tailwind output (plain) or `/static/css/autumn.<hash>.css` (release) |
