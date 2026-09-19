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
| **Forms & validation** (`ChangesetForm` round-trip — a rejected submission comes back with the author's input and one message per field) | `routes/posts.rs` (`submit`, `update`) |
| **Typed accessible form primitives** (`a11y::TextField` / `TextArea` / `Select` / `Button` / `Link`; an unlabeled field does not compile) | `routes/posts.rs` (`submit_form_markup`, `edit_form_markup`) |
| **Rich text** (user-submitted Markdown rendered through `markdown::render_user_content`) | `routes/posts.rs` (`show`) |
| **Cookie consent** (`inject_consent_banner`, POST accept/reject/withdraw, a preferences page, the `analytics` gate) | `routes/consent.rs`, `routes/layout.rs`, `main.rs` |
| **Pagination** (`PageRequest` + `Page` + `pagination_nav`, plain `<a href>` page links) | `routes/subreddits.rs` (`show`) |
| **No-JavaScript fallbacks** (forms and pagination carry no `hx-*` attributes; vote and comment controls degrade to form POSTs) | `routes/posts.rs`, `routes/subreddits.rs`, `routes/layout.rs` |
| **Failure capsules** (record a failing request, replay it offline with `autumn replay`) | `autumn-capsules.toml`, `capsules/`, `tests/failure_capsule.rs` |
| **Deterministic simulation testing** (`#[sim_test]`, virtual clock, `always!` / `sometimes!`) | `tests/sim_hot_rank.rs` |

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

## Failure capsules: record → `autumn replay`

A stack trace tells you *where* a request died. A **failure capsule** tells you
*what it was doing*: the request that failed, the rows the database handed back,
the clock readings the handler took, and the response the client got — written
to one JSON file the moment the failure happens, and replayable offline. See
[`docs/guide/failure-capsules.md`](../../docs/guide/failure-capsules.md).

Capture is off by default and lives in its own profile here, because turning it
on is a deliberate decision rather than a dev convenience — see "What a capsule
holds" below.

```bash
docker compose up -d
AUTUMN_PROFILE=capsules cargo run -p reddit-clone
```

`autumn-capsules.toml` is a *custom* profile, not a dev overlay, so it carries
the same database URL and file mail transport as `autumn-dev.toml` plus an
explicit `auto_migrate = true` — only `dev` auto-applies migrations by
convention. The dev error overlay and detailed health are dev-profile
conveniences and stay off here; the capsule on disk is the artifact this
profile exists for.

Make it fail. `/dev/trigger-error` propagates a real `parse::<i32>()` error
through `?`, so it is a genuine 500 rather than a hand-written one:

```bash
curl -i http://localhost:3000/dev/trigger-error   # 500
ls tmp/autumn-capsules/
# 20260812T101413.882104-000000-01JB2K7Q.json
```

One file per failing request. A `4xx` writes nothing and a successful request
drops its buffer at the response boundary — capsules are for failures only.

Replay it. The CLI compiles *your* app (only your app knows its routes, state
and config) and runs it with the database served from the capsule's tape, the
clock serving the recorded readings, outbound HTTP refused and no port bound:

```bash
autumn replay -p reddit-clone tmp/autumn-capsules/<file>.json
```

```text
REPRODUCED  /…/tmp/autumn-capsules/<file>.json
  expected: 500 invalid digit found in string
  actual:   500 invalid digit found in string
```

The verdict is JSON on stdout and the human summary on stderr, so
`autumn replay … | jq` works while you still read the summary. Four verdicts,
and the exit code matches: `reproduced` (0) — the bug is still there;
`mismatch` (1) — usually what you want *after* a fix; `diverged` (1) — your
code asked the database something the recording never asked, so a matching
status would have been luck; `refused` (2) — nothing was replayed, because the
capsule is truncated or its body was never recorded.

### The committed capsule

`capsules/dev-trigger-error.json` is a real capsule recorded from that route,
committed so the walkthrough has something to point at without a database.
`tests/failure_capsule.rs` parses it through the same `Capsule::from_json` the
replay CLI uses and asserts the shape this section describes, and re-records it
on demand:

```bash
cargo test -p reddit-clone --test failure_capsule
UPDATE_CAPSULE_FIXTURE=1 cargo test -p reddit-clone --test failure_capsule
```

See [`capsules/README.md`](capsules/README.md) for why that particular capsule
is safe to commit and yours is not.

### What a capsule holds

**A capsule is production data.** It is a copy of what one of your users sent
and what your database sent back. Autumn masks what it can identify by *name*,
through the same `[log] filter_parameters` list the access log uses — but
**database result rows are raw `PostgreSQL` protocol bytes and are not masked**,
because replay depends on them being exact and Autumn has no idea which column
is a national ID. `tmp/autumn-capsules` is gitignored at the workspace root for
that reason; do not serve it, and treat a capsule you move off a host the way
you would treat the original.

Redaction is matched by **equality** after normalization, never by prefix — so
`authorization` and `cookie` are covered out of the box, while a prefixed header
like this app's `Stripe-Signature` intake header is not. That is why
`autumn-capsules.toml` adds it (and `x-api-key`, `x-auth-token`) to
`[log] filter_parameters`, and why `tests/failure_capsule.rs` asserts it stays
there.

Two limits worth knowing before you record against a real route here:
authenticated and CSRF-protected routes do **not** replay faithfully (the
`authorization` and `cookie` headers are masked, so the replayed request meets
the auth layer without credentials and stops at a `401`/`403` — the capsule is
still a faithful record, just not a re-runnable one), and a handler that draws
from `Rng` draws different bytes on replay, which shows up as a bind divergence
if those bytes reach a SQL bind.

## Deterministic simulation testing

`tests/sim_hot_rank.rs` carries a seeded `#[sim_test]` over this app's
hot-rank decay curve — the `score / (age_hours + 2)^1.5` formula in `tasks.rs`.

Every interesting property of that formula is a statement about time passing,
which a conventional test cannot express without either sleeping for a day or
bypassing the seam that decides what "now" means. `#[sim_test]` hands the test a
seeded `Sim` with a **virtual clock** that the mounted app reads through the
ordinary `Clock` extractor, so `sim.advance(24h)` ages the app by a day with
zero wall-clock sleeping — through the seam, not around it.

```bash
cargo test -p reddit-clone --test sim_hot_rank              # 48 virtual hours, ~20ms
AUTUMN_SIM_SEED=0x9f3a cargo test -p reddit-clone --test sim_hot_rank
```

The second form is the replay line `#[sim_test]` prints on failure: the scores
are drawn from `sim.rng()`, so a seed reproduces a run bit for bit. The test
uses both assertion macros for what each is for — `always!` for the hard
invariants (a rank never climbs as a post ages; a positive score never decays to
zero) and `sometimes!` for reachability, with an explicit
`assert_all_sometimes_satisfied()` so a green run is provably non-vacuous. See
[`docs/guide/simulation-testing.md`](../../docs/guide/simulation-testing.md).

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

## UI: forms, accessibility, rich text, consent and pagination

The post submit/edit pair is the app's showcase for the framework's UI surface,
and each piece has a guide:

```bash
# Typed a11y controls: every field has a real <label>, and an invalid one
# carries aria-invalid + aria-describedby pointing at a role="alert" message.
curl -s http://localhost:3000/submit | grep -E '<label|aria-invalid|aria-describedby'

# No JavaScript required: the form has no hx-* attributes at all.
curl -s http://localhost:3000/submit | grep -c 'hx-'   # 0 inside the <form>

# Offset pagination, with plain <a href> page links.
curl -s 'http://localhost:3000/r/rust?page=2' | grep -E '<nav aria-label="Pagination"|aria-current'

# The consent banner, until a choice is recorded.
curl -s http://localhost:3000/ | grep 'autumn-consent-banner'
```

| Part | File | Guide |
|------|------|-------|
| `ChangesetForm` round-trip, re-rendering a 422 with the author's input | `src/routes/posts.rs` | [`forms.md`](../../docs/guide/forms.md) |
| Typed accessible controls and their error wiring | `src/routes/posts.rs` | [`accessibility.md`](../../docs/guide/accessibility.md) |
| Sanitized user-submitted Markdown post bodies | `src/routes/posts.rs` (`show`) | [`rich-text.md`](../../docs/guide/rich-text.md) |
| Consent routes, the banner layer, the `analytics` gate | `src/routes/consent.rs`, `src/main.rs` | [`cookie-consent.md`](../../docs/guide/cookie-consent.md) |
| `PageRequest` + `pagination_nav` on the community listing | `src/routes/subreddits.rs` | [`pagination.md`](../../docs/guide/pagination.md) |

Two details in there are worth reading the code for. Rich text is sanitized at
**render** time, not on write, so the database keeps the author's original
Markdown (an edit shows them what they typed) and a later allowlist change
protects posts already written. And the community page wires its live SSE feed
on **page 1 only** — appending a just-published post to page 2 would show the
reader a row that is not part of the slice they asked for.

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
