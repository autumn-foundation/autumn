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
| GET    | `/actuator/metrics`      | Request and pool stats |
| GET    | `/health`                | Health check           |
| GET    | `/static/js/htmx.min.js` | Bundled htmx          |
| GET    | `/static/css/autumn.css` | Compiled Tailwind CSS  |

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
