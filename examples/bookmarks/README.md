# Autumn Bookmarks Example

A bookmark manager regenerated from:

```bash
autumn generate scaffold Bookmark url:String title:String tag:String alive:bool \
  --index url \
  --index tag \
  --validate url=url \
  --validate title=length:min=1,max=200 \
  --default alive=true \
  --query find_by_tag:tag \
  --query find_by_alive:alive
```

The shipped example then layers on profile-aware configuration, scheduled
tasks, embedded migrations, htmx, and actuator endpoints.

## What it demonstrates

| Feature | Where | What it does |
|---------|-------|--------------|
| **Profiles** | `autumn.toml` + `autumn-dev.toml` | Dev profile auto-detected; DB URL only in dev config |
| **`#[model]`** | `src/models/bookmark.rs` | Generates `Bookmark`, `NewBookmark`, `UpdateBookmark` from one struct |
| **`#[repository]`** | `src/repositories/bookmark.rs` | Generates `PgBookmarkRepository` with CRUD + `find_by_tag` + REST handlers |
| **Scheduled tasks** | `src/tasks.rs` | `#[scheduled(every = "1h")]` link health checker |
| **Embedded migrations** | `src/main.rs` | Runs Diesel migrations at startup |
| **Actuator** | Nav bar links | `/actuator/health`, `/actuator/info` auto-mounted |
| **OpenAPI export** | `src/main.rs` | `.openapi(..)` serves `/openapi.json`; `autumn openapi export` writes the same document without booting the app, ready for a client generator |
| **App metrics facade** | `src/metrics.rs`, `src/routes/bookmarks.rs` | One domain counter and one timer recorded at the call site with `autumn_web::metrics` — no type to define, nothing registered with `AppBuilder` — landing on the same `/actuator/prometheus` scrape as the built-in `autumn_http_*` families |

## Prerequisites

- Rust (edition 2024)
- Docker & Docker Compose (for Postgres)

## Quick start

From the **workspace root** (`autumn/`):

```bash
# 1. Download Tailwind CSS CLI
cargo run -p autumn-cli -- setup

# 2. Start Postgres
docker compose -f examples/bookmarks/docker-compose.yml up -d

# 3. Run the application (dev profile auto-detected)
cargo run -p bookmarks
```

The server starts at <http://localhost:3000>.

## Available routes

### HTML (browser)

| Method | Path         | Description                  |
|--------|--------------|------------------------------|
| GET    | `/`          | Redirect to `/bookmarks`     |
| GET    | `/bookmarks` | List all bookmarks           |
| GET    | `/bookmarks/export.csv` | Download all bookmarks as CSV (typed `Download`, Range/206) |
| GET    | `/bookmarks/stats` | Grouped-aggregate roll-ups: top tags + added-per-day series |
| GET    | `/bookmarks/{id}` | Show one bookmark        |
| GET    | `/bookmarks/tag/{tag}` | Filter bookmarks by tag |
| GET    | `/bookmarks/new` | Add bookmark form        |
| GET    | `/bookmarks/{id}/edit` | Edit bookmark form   |

### JSON API

These routes are generated from `#[autumn_web::repository(Bookmark, api = "/api/bookmarks")]`.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/bookmarks` | List all bookmarks |
| GET | `/api/bookmarks/{id}` | Fetch one bookmark |
| POST | `/api/bookmarks` | Create a bookmark |
| PUT | `/api/bookmarks/{id}` | Update a bookmark |
| DELETE | `/api/bookmarks/{id}` | Delete a bookmark |

### Framework

| Method | Path                     | Description            |
|--------|--------------------------|------------------------|
| GET    | `/actuator/health`       | Health + profile info  |
| GET    | `/actuator/info`         | Build & runtime info   |
| GET    | `/actuator/metrics`      | Request and pool stats, plus this app's own metrics under `app` |
| GET    | `/actuator/prometheus`   | Prometheus scrape, built-in and app families together |
| GET    | `/health`                | Health check           |
| GET    | `/static/js/htmx.min.js` | Bundled htmx          |
| GET    | `/static/css/autumn.css` | Compiled Tailwind CSS  |

## App metrics

`/actuator/prometheus` already exposed the framework's own `autumn_http_*`
families. `autumn_web::metrics` lets **this app** add its own instruments at the
point where the interesting thing happens — one line at the call site, no trait
to implement, no type to define, nothing to register with `AppBuilder`. See
[`docs/guide/metrics.md`](../../docs/guide/metrics.md).

`src/metrics.rs` is the one place that names an instrument, so the call sites
stay one line each:

| Instrument | Kind | Recorded where | Answers |
|---|---|---|---|
| `bookmarks_created_total{outcome}` | counter | `routes::bookmarks::create` | *how many submissions did the form accept, and how many did it reject?* |
| `bookmark_stats_query_seconds` | timer (histogram) | `routes::bookmarks::stats` | *how long do the two grouped aggregates behind `/bookmarks/stats` take?* |

```bash
curl -s localhost:3000/actuator/prometheus | grep -E 'bookmark(s_created|_stats)'
```

```text
# HELP bookmarks_created_total Bookmarks submitted through the create form, by outcome
# TYPE bookmarks_created_total counter
bookmarks_created_total{outcome="created"} 3
# TYPE bookmark_stats_query_seconds histogram
bookmark_stats_query_seconds_bucket{le="0.025"} 2
...
bookmark_stats_query_seconds_count 2
```

Three details worth copying into your own app:

- **The counter counts both outcomes.** A rejected submission is its own
  series, not a lost sample, so
  `rate(bookmarks_created_total{outcome="rejected"}[5m])` can alert on a form
  that suddenly stops validating.
- **The timer's guard records on drop**, so every exit path is covered
  including an early `?`. `stats` resolves it explicitly with `stop()` after the
  last query, so the histogram measures the database work rather than the markup
  rendering that follows. Bind the guard to a named variable — `let _ = …` drops
  it immediately and records roughly zero.
- **`describe()` runs once in `main`**, before the server starts, because
  bucket bounds are frozen at registration and cannot move under a running
  scrape target. Describing does not register, so the two calls may come in
  either order.

Labels must come from a small, closed set the code controls — `outcome` here is
two values. Never label with user input, a tag, or a bookmark id: every distinct
combination is a separate series that lives for the life of the process.

The assertions live in `src/metrics.rs`'s own test module and need no database:

```bash
cargo test -p bookmarks --bin bookmarks
```

## Seeding fake data

`src/bin/seed.rs` uses the `#[model]` factory's `.fake()` support (issue #1343)
to fill the database with realistic rows so you can exercise pagination and
search against a populated list.

```bash
# Populate 200 faked bookmarks in one shot (the seed binary's default body).
# Idempotent: it only bulk-seeds when the table is empty.
autumn seed
```

The one line that does the work in `src/bin/seed.rs`:

```rust
Bookmark::factory().fake().create_many(200, ctx.pool()).await;
```

You can also generate a specific count for any registered `#[model]` without
editing the seed binary:

```bash
# Insert 200 faked Bookmark rows to try pagination/search on a full list.
autumn seed --count 200 --model Bookmark
```

`autumn seed` forwards `--count`/`--model` to the seed binary, which routes them
to `autumn_web::seed::fake_seed_model` — the model is looked up by name from a
registry every `#[autumn_web::model]` joins automatically.

## Try the generated CRUD API

```bash
# Create
curl -X POST http://localhost:3000/api/bookmarks \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://rust-lang.org","title":"Rust","tag":"lang","alive":true}'

# List
curl http://localhost:3000/api/bookmarks

# Update
curl -X PUT http://localhost:3000/api/bookmarks/1 \
  -H 'Content-Type: application/json' \
  -d '{"title":"Rust Lang","tag":"rust","alive":true}'
```

## Generate a typed client from the spec

The app configures `.openapi(OpenApiConfig::new("Bookmarks API", "1.0.0"))`, so
the whole `/api/bookmarks` surface above is described by a machine-readable
contract. Get it out without starting the server:

```bash
cargo run -p autumn-cli -- openapi export -p bookmarks --out openapi.json
```

That compiles the app and runs it in a dump mode — no port bound, no database
touched — emitting the same document `/openapi.json` serves. Because `Bookmark`,
`NewBookmark` and `UpdateBookmark` are `#[model]` types, they register their own
schemas, so the export carries real fields rather than opaque objects. Confirm
that with:

```bash
cargo run -p autumn-cli -- openapi export -p bookmarks --strict >/dev/null
```

`--strict` fails if any type on the API boundary would reach a generated client
as an untyped blob.

Then hand the document to whichever generator you prefer:

```bash
npx openapi-typescript openapi.json -o src/api.d.ts   # TypeScript
cargo progenitor -i openapi.json -o ./client -n bookmarks-client   # Rust
```

In CI, `autumn openapi export --check openapi.json --strict` fails when the
committed contract no longer matches the handlers.
