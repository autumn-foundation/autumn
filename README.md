# Autumn 🍂

[![CI](https://github.com/madmax983/autumn/actions/workflows/ci.yml/badge.svg)](https://github.com/madmax983/autumn/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/madmax983/autumn/branch/trunk/graph/badge.svg)](https://codecov.io/gh/madmax983/autumn)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Rust: 1.88.0+](https://img.shields.io/badge/rust-1.88.0%2B-orange.svg)](https://www.rust-lang.org)
[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/madmax983/autumn)

> Spring Boot-style web framework for Rust, built on [Axum](https://github.com/tokio-rs/axum).

Autumn assembles proven Rust crates into a convention-over-configuration web
stack with proc-macro ergonomics, framework defaults, and customization options when
you need them. If Spring Boot, Rails, or Laravel feels familiar, Autumn aims
for that same "ship the app, not the plumbing" shape in Rust.

## Features

- **Route and app macros** - `#[get]`, `#[post]`, `#[put]`, `#[delete]`, `routes![]`, `#[autumn_web::main]`
- **Pre-rendering pages to static HTML** - `#[static_get]` + `static_routes![]` with `autumn build` pre-rendering to `dist/`
- **Application builder** - `.routes()`, `.tasks()`, `.static_routes()`, `.scoped()`, `.merge()`, and `.nest()`
- **Configuration and profiles** - defaults, `autumn.toml`, `autumn-{profile}.toml`, and `AUTUMN_*` overrides
- **Database ergonomics** - async Postgres primary/replica pools, `Db` extractor for the primary/write role, `#[model]`, `#[repository]`, hooks, and embedded migrations
- **HTML stack** - Maud templating, bundled htmx, Tailwind build pipeline, and static asset serving
- **Operations** - `/health`, `/actuator/*`, structured logging, metrics, and graceful shutdown
- **Background work** - `#[scheduled]` tasks, `#[job]` handlers, one-off `#[task]` scripts via `autumn task`, and runtime task visibility at `/actuator/tasks`
- **Companion workflows** - [Autumn Harvest](docs/autumn-workflow-architecture.md) is the separate durable workflow engine for multi-step orchestration when `#[scheduled]` or `#[job]` is not enough
- **Transactional email** - optional `mail` feature with Maud templates, log/file/SMTP transports, and a `Mailer` extractor
- **Security primitives** - session cookies, auth extractor, security headers, CSRF, and `#[secured]`
- **File storage (optional)** - pluggable `BlobStore` trait with built-in `Local` and S3-compatible backends, HMAC-signed URLs, and `MultipartField::save_to_blob_store` (see [storage guide](docs/guide/storage.md))
- **CLI workflow** - `autumn new`, `autumn setup`, `autumn dev`, `autumn build`, `autumn migrate`, and `autumn task`

## Quickstart

```bash
# Install the published CLI
cargo install autumn-cli --version 0.6.0

# Local development only, from an Autumn checkout:
# cargo install --path autumn-cli

# Create a new project
autumn new my-app
cd my-app

# Optional: download Tailwind CSS for styled builds
autumn setup

# Optional: scaffold a CRUD resource (see docs/guide/generators.md)
# autumn generate scaffold Post title:String body:Text published:bool

# Development server with file watching
autumn dev

# Or run without watch mode
# cargo run
```

Visit <http://localhost:3000>. Autumn also auto-mounts `/health`,
`/actuator/health`, `/actuator/info`, and `/static/js/htmx.min.js`.

### Watching custom directories

`autumn dev` always watches `src/`, `static/`, `templates/`, and `migrations/`
plus the project's top-level config files (`autumn.toml`, `Cargo.toml`,
`Cargo.lock`, `build.rs`, `tailwind.config.js`). To watch additional folders
(for example, custom view or locale trees), add a `[dev]` section to
`autumn.toml`:

```toml
[dev]
watch_dirs = ["views", "locales"]
```

Listed directories are watched recursively in addition to the defaults.
Multi-segment paths like `content/locales` are supported. Changes inside
them trigger a server restart and a full browser reload. Paths under
`target/` and dotted directories are still ignored.

Entries must be project-relative; absolute paths, `..` traversal,
`target`, and dotted directories (e.g. `.git`) are rejected with a
warning. Missing directories are skipped at startup.

If you add `#[static_get]` routes, `autumn build` pre-renders them into
`dist/`.

## Local-Safe vs Production-Safe

Autumn still distinguishes between "works on your laptop" and "safe to run in a
multi-replica deployment":

- Local-safe defaults: in-memory sessions, pretty logs in `dev`, `scheduler.backend = "in_process"` for `#[scheduled]`, single-binary startup, and no inbound request deadline (so a debugger pause never 503s you).
- Production-safe options: `/live`, `/ready`, `/startup` probes, OTLP telemetry config, Redis-backed sessions, Redis-backed channels/jobs, Postgres-coordinated scheduled tasks, container scaffolding from `autumn new`, explicit migration jobs before web replicas roll, and a built-in **inbound request timeout** (the `prod` profile smart-defaults `server.timeouts.request_timeout_ms = 30000`) so a single hung handler returns a clean `503` and frees its worker instead of starving the pool — no hand-written tower layers. Streaming responses (SSE) are never interrupted — the deadline bounds the response head, not the body stream — and WebSocket upgrades are bounded only for the handshake, never the live socket. Any route can override with `#[get("/export", timeout_ms = 120000)]` or `timeout = "off"`.

If you are deploying beyond a single process, read the
[Cloud-Native Guide](docs/guide/cloud-native.md) before treating the defaults as
done.

## Database Topologies

Autumn supports three explicit database shapes:

- **Single primary**: set `database.url` or `database.primary_url`. Writes,
  transactions, advisory locks, and `autumn migrate` use that primary role.
- **Primary plus read replica**: set `database.primary_url` and
  `database.replica_url`, with optional `primary_pool_size`,
  `replica_pool_size`, and `replica_fallback = "fail_readiness"` or
  `"primary"`.
- **One-shot migrator path**: run `autumn migrate` once against the primary
  before rolling web replicas. Production web replicas should keep
  `auto_migrate_in_production = false`.

`database.url` and `DATABASE_URL` remain valid for existing single-URL apps.
For new production config, prefer `AUTUMN_DATABASE__PRIMARY_URL` so the write
role is named plainly. `autumn doctor --strict` reports missing primaries,
unsafe production startup migrations, role connectivity failures, and stale
replica migrations without printing credentials.

## Autumn Harvest

Autumn Harvest is the companion workflow engine for durable, multi-step work:
workflow history, activity retries, timers, singleton orchestration, and
long-running business processes. It is intentionally a separate release train
from `autumn-web`: Harvest can depend on Autumn Web's `AppState` and builder
surface, but Autumn Web's examples and tests should not need Harvest in order to
ship a web release. That keeps the dependency graph pointed in one direction
instead of forming a circular release dependency.

Use built-in `#[scheduled]` tasks and `#[job]` handlers for lightweight app-local
background work. Reach for Harvest when the work needs workflow durability or a
dedicated runner. See the [Harvest architecture notes](docs/autumn-workflow-architecture.md)
for the model and roadmap.

## Example

This is the small-app shape Autumn is built around:

```rust
use autumn_web::prelude::*;

#[get("/")]
async fn index() -> &'static str {
    "Welcome to Autumn!"
}

#[get("/hello/{name}")]
async fn hello_name(name: autumn_web::extract::Path<String>) -> String {
    format!("Hello, {}!", *name)
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![index, hello_name])
        .run()
        .await;
}
```

## Built On

- [Axum](https://github.com/tokio-rs/axum) - async HTTP routing and middleware
- [Diesel](https://diesel.rs/) + [diesel-async](https://github.com/weiznich/diesel_async) - database access
- [Maud](https://maud.lambda.xyz/) - compiled HTML templates
- [htmx](https://htmx.org/) - HTML-first interactivity
- [Tailwind CSS](https://tailwindcss.com/) - utility-first styling
- [Tokio](https://tokio.rs/) - async runtime
- [Tracing](https://github.com/tokio-rs/tracing) - structured logging

## Examples

See [EXAMPLES.md](EXAMPLES.md) for the full catalog with personas, journeys, prerequisites, run commands, and success proofs.

| Example | Description |
|---------|-------------|
| [`examples/hello`](examples/hello) | Minimal hello-world app with route macros and no database |
| [`examples/todo-app`](examples/todo-app) | Full-stack CRUD app with Diesel, Maud, htmx, Tailwind, JSON API, bearer-token auth, and MCP tool projection |
| [`examples/blog`](examples/blog) | Blog engine with admin UI, validation, and pre-rendering pages to static HTML via `#[static_get]` |
| [`examples/bookmarks`](examples/bookmarks) | Repository macro, generated CRUD API, profiles, scheduled tasks, and actuator endpoints |
| [`examples/bookmarks-distributed`](examples/bookmarks-distributed) | Primary/replica Postgres, multi-replica web tier behind nginx, advisory-lock scheduling, and Docker Compose deployment |
| [`examples/bookmarks-sharded`](examples/bookmarks-sharded) | Framework-native horizontal sharding: tenant → slot → shard routing, control database, cross-shard fan-out, and Docker Compose deployment |
| [`examples/wiki`](examples/wiki) | Mutation hooks, revision history, generated REST API, and slug lifecycle management |
| [`examples/reddit-clone`](examples/reddit-clone) | Canonical feature showcase: auth, sessions, CSRF, `#[secured]`, transactional email, `#[job]`, `#[ws]` channels, Redis fan-out, htmx voting, A/B experiments, signed webhook intake, outbound HTTP with SSRF protection, structured error reporting, and live-tunable config |
| [`examples/saas`](examples/saas) | Multi-tenant SaaS starter: session auth + row-level tenancy + tenant-scoped dashboard — the flagship `autumn new --starter saas` archetype (see the [starters guide](docs/guide/starters.md)) |

## Documentation

- [Getting Started Guide](docs/guide/getting-started.md)
- [Dev-Loop Latency Budget](docs/guide/dev-loop-latency.md) — p50/p95/max budgets per change class, measurement methodology, and CI gates for `autumn dev`
- [Signed Webhook Intake](docs/guide/signed-webhooks.md)
- [Docs Smoke Procedure](docs/guide/docs-smoke.md) - release gate for first-run docs
- [Release Checklist](docs/release-checklist.md)
- [Code Generators](docs/guide/generators.md) — `autumn generate model | migration | scaffold`
- [One-Off Tasks](docs/guide/tasks.md) - `#[task]`, `one_off_tasks![]`, and `autumn task`
- [Multi-Replica Scheduled Tasks](docs/guide/scheduled-multi-replica.md) - `#[scheduled]` with Postgres advisory-lock coordination
- [Horizontal Sharding](docs/guide/sharding.md) — `[[database.shards]]`, slot-based routing, `ShardedDb`/`Shards` extractors, per-shard health and migrations
- [Operating Background Jobs](docs/guide/operating-background-jobs.md) - admin dashboard and recovery actions for `#[job]`
- [Exposing Your API as MCP Tools](docs/guide/mcp.md) — project typed endpoints into a Model Context Protocol server with `#[api_doc(mcp)]` + `mount_mcp`
- [Mail Guide](docs/guide/mail.md)
- [View Formatting Helpers](docs/guide/format-helpers.md) — `number_to_currency`, `pluralize`, `truncate`, `time_ago_in_words`, and friends for Maud templates
- [Cloud-Native Guide](docs/guide/cloud-native.md)
- [Logging & PII](docs/guide/logging-pii.md)
- [Todo Tutorial](docs/guide/tutorial/index.md)
- [Autumn Harvest Architecture Notes](docs/autumn-workflow-architecture.md)
- [API Reference](https://docs.rs/autumn-web)
- [Pre-rendering Design Notes](docs/design/hybrid-rendering.md)
- [Stability Policy](STABILITY.md) — SemVer, MSRV, and migration commitments

## Stability

Autumn commits to [Semantic Versioning](https://semver.org) for its public
API starting at `1.0.0`. See [STABILITY.md](STABILITY.md) for the full
definition of the stable surface, the MSRV policy, and the migration-guide
process for future major releases.

Until `1.0.0`, Autumn is in its `0.x` series — see the
[pre-1.0 notes](STABILITY.md#pre-10-notes) for what that means in practice.

## Requirements

- Rust 1.88.0+ (edition 2024)
- PostgreSQL for database-backed apps

Autumn can still run without a database if you omit the `[database]` section.

## License

MIT OR Apache-2.0

