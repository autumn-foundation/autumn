# Reddit Clone

A Reddit clone built with [Autumn](https://github.com/autumn-foundation/autumn),
showcasing the framework's major features in a single cohesive application.

## Features Demonstrated

| Feature | Where |
|---------|-------|
| Route macros (`#[get]`, `#[post]`, `#[delete]`, `routes![]`) | All route files |
| `#[autumn_web::main]` entry point | `main.rs` |
| Hybrid rendering (`#[static_get]`, `static_routes![]`) | `routes/about.rs` |
| Configuration profiles (`autumn.toml` + `autumn-dev.toml`) | Project root |
| Database (Diesel async Postgres, `Db` extractor) | All route handlers |
| Embedded migrations | `main.rs`, `migrations/` |
| `#[model]` macro with `#[id]`, `#[indexed]`, `#[validate]`, `#[default]` | `models.rs` |
| `#[repository]` with derived queries and REST API generation | `repositories.rs` |
| Mutation hooks (`before_create`, `before_update`) | `hooks.rs` |
| Session cookies (`Session` extractor, `rotate_id`, `destroy`) | `routes/auth.rs` |
| Password hashing (`hash_password`, `verify_password`) | `routes/auth.rs` |
| `#[secured]` route protection | `routes/subreddits.rs`, `routes/posts.rs` |
| CSRF protection (`CsrfToken` + htmx header injection) | `routes/layout.rs`, all forms |
| Field validation (`#[validate(length(min, max))]`) | `models.rs` |
| Scheduled background tasks (`#[scheduled(every = "15m")]`) | `tasks.rs` |
| **Background Jobs** (`#[job]`, `jobs![]`, local/Redis runtime) | `jobs.rs`, `/actuator/jobs` |
| **WebSockets** (`#[ws]`, `Channels`, durable app-db relay, pluggable live-event bus, `CancellationToken`, relay health JSON) | `routes/live.rs`, `live_events.rs`, `live_bus.rs` |
| Actuator endpoints (`/health`, `/actuator/*`) | Auto-mounted |
| Maud HTML templates | All route files |
| htmx interactivity (voting, deletion, logout) | `routes/votes.rs`, `routes/layout.rs` |
| Tailwind CSS styling | All templates |
| Static asset serving (`/static/css/`, `/static/js/htmx.min.js`) | Auto-mounted |
| Audit logging (`AuditLogger` + `TracingAuditSink`, actor auto-attribution via `Current::actor()`) | `main.rs`, `routes/posts.rs` (`delete_post`) |
| **Route-level SEO** (`seo(...)` + `SeoMeta` extractor, canonical URLs, `robots = "noindex"`, DB-backed `SitemapSource`, auto-mounted `/robots.txt` + `/sitemap.xml`) | `seo.rs`, `autumn.toml`, `routes/posts.rs`, `routes/subreddits.rs`, `routes/about.rs`, `routes/layout.rs` |

## Prerequisites

- Rust 1.88.0+
- PostgreSQL and Redis (via Docker Compose below)

## Quick start

```bash
# Start PostgreSQL + Redis
docker compose up -d

# Run the app in dev mode
# The first boot applies the reddit-clone schema and starts the local job
# runtime plus the durable live-feed relay.
cargo run -p reddit-clone

# Optional: watch mode from the workspace root
# cargo run -p autumn-cli -- dev -p reddit-clone

# Visit http://localhost:3000
```

The local job backend is the zero-config default. Registration enqueues
`user_onboarding` to award starter karma. Post submission enqueues
`post_publication` to refresh `hot_rank`, store a durable live-feed event, and
wake connected feed relays.

Inspect job state through the actuator:

```bash
curl http://localhost:3000/actuator/jobs
```

The live-feed relay exposes an operator-facing JSON snapshot:

```bash
curl http://localhost:3000/api/live/relay/health
```

## Redis Jobs And Live Feed

For a local Redis-backed queue and cross-process wakeup demo, use the checked-in
Redis profile:

```bash
docker compose up -d
AUTUMN_PROFILE=redis cargo run -p reddit-clone
```

That profile lives in `autumn-redis.toml` and configures:

- `jobs.backend = "redis"` for durable ad-hoc jobs
- `distributed.live_feed_bus.kind = "redis_pubsub"` for live-feed wakeups

The live WebSocket feed keeps the app database as durable truth via
`live_feed_events`, while the wakeup path is pluggable:

- Default/dev mode uses Postgres `LISTEN/NOTIFY`
- Redis profile uses Redis pub/sub for cross-process wakeups
- Redis mode also keeps Postgres `NOTIFY` as a safety net, so missed Redis
  publishes still wake web nodes from the durable event log
- Polling is the last fallback when neither wake path is available

## Why This Example Uses Jobs Instead Of Harvest

Autumn Harvest is still the companion workflow engine for durable, multi-step
orchestration: workflow history, activity retries, timers, and dedicated
runners. This example uses Autumn Web's built-in `#[job]` runtime for the
registration and post-publication side effects because those are small
request-triggered jobs, not long-running workflows.

Keeping reddit-clone off `autumn-harvest` also keeps the release train clean.
Harvest depends on Autumn Web integration points; Autumn Web should not require
Harvest in a checked-in example just to publish a web release. See
[`docs/autumn-workflow-architecture.md`](../../docs/autumn-workflow-architecture.md)
when your app needs the heavier workflow machinery.

## Live Feed Operations

`/api/live/relay/health` reports the current relay state for the local process.
The important fields are:

- `listener_state`: which wake path is currently active (`postgres`, `redis`, `redis+postgres`, or `polling`)
- `reconnect_attempts` / `reconnect_successes` / `reconnect_failures`: whether the process is healing broken listeners
- `wake_redis`, `wake_postgres`, `wake_poll`: which path is waking the relay
- `replayed_events`, `last_seen_id`, `last_replayed_at`: whether durable rows are still flowing through replay
- `last_error`: the last relay or publish error seen by this process

Operator heuristics:

- Sustained growth in `wake_poll` means the process is living on fallback instead of a real bus.
- Growing `reconnect_failures` with a flat `reconnect_successes` means the configured wake path is still broken.
- A stale `last_replayed_at` while app writes continue means live updates are stuck before rebroadcast.
- In Redis mode, `listener_state = "redis+postgres"` is healthy: Redis is primary and Postgres is the backup wake path.

## WebSocket Live Feed

Connect to the live activity feed for real-time notifications:

```bash
# Global feed (all activity)
websocat ws://localhost:3000/ws/feed

# Subreddit-specific feed
websocat ws://localhost:3000/ws/r/rustlang
```

## SEO

`[seo] base_url` in `autumn.toml` mounts `/robots.txt` and `/sitemap.xml`. The
sitemap source in `src/seo.rs` reads the database at start-up and lists the
front page, the community index, the communities, and the posts (capped at
1,000 and 5,000 entries respectively, with a logged warning when a cap bites).
The caps bound the number of URLs, not the work the query does — see
`src/seo.rs` for when to stop building the sitemap at boot.

```bash
# The crawl rules. The dev profile disallows every crawler; prod allows them.
curl http://localhost:3000/robots.txt

# One <url> per public page, with <lastmod> from posts.updated_at.
curl http://localhost:3000/sitemap.xml

# The per-page meta tags the route attributes declare.
curl -s http://localhost:3000/ | grep -E '<title>|og:|canonical'
curl -s http://localhost:3000/about | grep -E '<title>|og:'
```

Where each part lives:

| Part | File |
|------|------|
| `[seo]` and `[seo.robots]` settings | `autumn.toml` |
| `SitemapSource`, canonical helpers, `summarize` | `src/seo.rs` |
| Attribute-only meta tags on a static page | `src/routes/about.rs` |
| Attribute defaults refined from a database row | `src/routes/posts.rs` (`show`) |
| `robots = "noindex, nofollow"` on a private page | `src/routes/posts.rs` (`submit_form`) |
| `SeoMeta::render()` in the shared `<head>` | `src/routes/layout.rs` |

Change `base_url` to the real host before you deploy. See
[`docs/guide/seo.md`](../../docs/guide/seo.md) for the full guide.

## API Endpoints

The `#[repository]` macro auto-generates read-only REST endpoints:

```bash
# Subreddits
curl http://localhost:3000/api/subreddits
curl http://localhost:3000/api/subreddits/1

# Posts
curl http://localhost:3000/api/posts
curl http://localhost:3000/api/posts/1
```

## Architecture

```
src/
  main.rs           # App builder, route + task + job + WS registration, migrations
  models.rs         # #[model] structs: User, Subreddit (commentable), Post
                    #   (votable + commentable), Tag, Vote
  schema.rs         # Diesel table definitions
  repositories.rs   # #[repository] with derived queries and API generation
  hooks.rs          # MutationHooks for post lifecycle (auto-slug)
  jobs.rs           # #[job] onboarding + post-publication side effects
  live_bus.rs       # Live-feed bus config and backend selection
  live_events.rs    # Durable app-db live-feed relay with Postgres/Redis wakeups
  seo.rs            # SitemapSource (DB-backed), canonical-URL helpers, summarize()
  tasks.rs          # Scheduled hot-rank + live-feed retention tasks
  slugify.rs        # URL slug generation utility
  routes/
    mod.rs          # Module exports
    layout.rs       # Shared layout, vote controls, CSRF injection, time formatting
    auth.rs         # Register, login, logout, user profiles
    subreddits.rs   # Community listing, creation (#[secured]), detail view + community discussion
    posts.rs        # Front page, submit, view, edit, delete + the post's comment thread
    votes.rs        # htmx-powered upvote/downvote with toggle + ON CONFLICT
    live.rs         # #[ws] WebSocket feeds consuming process-local Channels
    about.rs        # #[static_get] pre-rendered about page
```
