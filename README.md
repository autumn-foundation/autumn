# Autumn 🍂

[![CI](https://github.com/autumn-foundation/autumn/actions/workflows/ci.yml/badge.svg)](https://github.com/autumn-foundation/autumn/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/autumn-foundation/autumn/branch/trunk/graph/badge.svg)](https://codecov.io/gh/autumn-foundation/autumn)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](#license)
[![Rust: 1.88.0+](https://img.shields.io/badge/rust-1.88.0%2B-orange.svg)](https://www.rust-lang.org)
[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/autumn-foundation/autumn)

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
- **CLI workflow** - `autumn new`, `autumn setup`, `autumn dev`, `autumn build`, `autumn migrate`, `autumn console`, and `autumn task`

## Quickstart

```bash
# Install the published CLI
cargo install autumn-cli --version 0.7.0

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

Need to poke at your data? `autumn console` scaffolds and runs a pre-wired
playground binary — same config, same database URL resolution, same pool as the
app (see the [data playground guide](docs/guide/console.md)):

```bash
autumn console
```

Visit <http://localhost:3000>. Autumn also auto-mounts `/health`,
`/actuator/health`, `/actuator/info`, and `/static/js/htmx.min.js`.

### Install a prebuilt binary (macOS & Linux)

Get the `autumn` CLI without compiling from source — no Rust toolchain required:

```sh
curl -fsSL https://raw.githubusercontent.com/autumn-foundation/autumn/trunk-dev/scripts/install.sh | sh
```

The installer detects your OS and architecture (macOS or Linux, x86_64 or aarch64), downloads the matching prebuilt binary, verifies its sha256 checksum, and installs it to `~/.local/bin/autumn` — printing the line to add if that directory isn't on your `PATH`. Override the target dir with `AUTUMN_INSTALL_DIR`, or pin a version with `AUTUMN_VERSION=vX.Y.Z` (or `--version vX.Y.Z`).

Prefer a manual download? Grab the tarball plus its `.sha256`:

- Latest: `https://github.com/autumn-foundation/autumn/releases/latest/download/autumn-<target>.tar.gz`
- Pinned: `https://github.com/autumn-foundation/autumn/releases/download/<tag>/autumn-<target>.tar.gz`

where `<target>` is one of `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-apple-darwin`, or `aarch64-apple-darwin` (Linux binaries are static musl — no glibc version dependency). Binaries track tagged crate releases (e.g. `v0.7.0`); `latest` is the most recent released version — there are no rolling trunk-dev builds.

### Install a prebuilt binary (Windows)

Windows ships a native `x86_64-pc-windows-msvc` build as a `.zip` (not a tarball). The POSIX `install.sh` above does not run on Windows; use the PowerShell installer instead:

```powershell
irm https://raw.githubusercontent.com/autumn-foundation/autumn/trunk-dev/scripts/install.ps1 | iex
```

It downloads `autumn-x86_64-pc-windows-msvc.zip`, verifies its sha256 (`Get-FileHash`), extracts `autumn.exe` to `%LOCALAPPDATA%\autumn\bin`, and prints the line to add if that directory isn't on your `PATH`. Override the install dir with `-Dir`, or pin a version with `-Version vX.Y.Z`.

Prefer a manual download? Grab the zip plus its `.sha256`:

- Latest: `https://github.com/autumn-foundation/autumn/releases/latest/download/autumn-x86_64-pc-windows-msvc.zip`
- Pinned: `https://github.com/autumn-foundation/autumn/releases/download/<tag>/autumn-x86_64-pc-windows-msvc.zip`

Verify with `Get-FileHash autumn-x86_64-pc-windows-msvc.zip -Algorithm SHA256` and compare against the `.sha256` file, then extract `autumn.exe` and put it on your `PATH`.

Prefer building from source? `cargo install --path autumn-cli` still works.

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
- Production-safe options: `/live`, `/ready`, `/startup` probes, OTLP telemetry config, Redis-backed sessions, Redis-backed channels/jobs, Postgres-coordinated scheduled tasks, an experimental zero-dependency [embedded 2-node cluster](docs/guide/clustering.md) (authenticated gossip membership plus an eventually consistent cluster-wide counter — an additional option for a shared counter without standing up Redis, not a production-safe substitute for the backends above and not a leader-election or mutual-exclusion primitive), container scaffolding from `autumn new`, explicit migration jobs before web replicas roll, and a built-in **inbound request timeout** (the `prod` profile smart-defaults `server.timeouts.request_timeout_ms = 30000`) so a single hung handler returns a clean `503` and frees its worker instead of starving the pool — no hand-written tower layers. Streaming responses (SSE) are never interrupted — the deadline bounds the response head, not the body stream — and WebSocket upgrades are bounded only for the handshake, never the live socket. Any route can override with `#[get("/export", timeout_ms = 120000)]` or `timeout = "off"`.

Deploys can also carry a **proven capacity contract**: `autumn calibrate` measures
what a build sustains and records `capacity.lock`, `autumn calibrate --check`
gates rebuilds against it in CI, and `[server] capacity_contract` lets the binary
admission-control against its own proven envelope instead of a hand-tuned guess
(see [Capacity Contracts](docs/guide/capacity-contracts.md)).

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
| [`examples/flock`](examples/flock) | WASM island spike: a server-rendered maud page whose home route mounts a Yew CSR "literary boids" widget compiled to `wasm32-unknown-unknown`, with a custom `'wasm-unsafe-eval'` CSP |
| [`examples/todo-app`](examples/todo-app) | Full-stack CRUD app with Diesel, Maud, htmx, Tailwind, JSON API, bearer-token auth, and MCP tool projection |
| [`examples/blog`](examples/blog) | Blog engine with admin UI, validation, and pre-rendering pages to static HTML via `#[static_get]` |
| [`examples/bookmarks`](examples/bookmarks) | Repository macro, generated CRUD API, profiles, scheduled tasks, actuator endpoints, and app metrics (a domain counter + timer on `/actuator/prometheus`) |
| [`examples/bookmarks-distributed`](examples/bookmarks-distributed) | Primary/replica Postgres, multi-replica web tier behind nginx, advisory-lock scheduling, a two-node self-clustering substrate needing no coordination service, and Docker Compose deployment |
| [`examples/bookmarks-sharded`](examples/bookmarks-sharded) | Framework-native horizontal sharding: tenant → slot → shard routing, control database, cross-shard fan-out, and Docker Compose deployment |
| [`examples/wiki`](examples/wiki) | Mutation hooks, revision history, generated REST API, and slug lifecycle management |
| [`examples/reddit-clone`](examples/reddit-clone) | Canonical feature showcase: auth, sessions, CSRF, `#[secured]`, transactional email, `#[job]`, `#[ws]` channels, Redis fan-out, htmx voting, A/B experiments, signed webhook intake, outbound HTTP with SSRF protection, structured error reporting, route-level SEO (`seo(...)`, `sitemap.xml`, `robots.txt`), typed accessible forms, sanitized user-submitted rich text, cookie consent, offset pagination, live-tunable config, failure capsules with an `autumn replay` walkthrough, and a seeded `#[sim_test]` |
| [`examples/saas`](examples/saas) | Multi-tenant SaaS starter: session auth + row-level tenancy + tenant-scoped dashboard — the flagship `autumn new --starter saas` archetype (see the [starters guide](docs/guide/starters.md)) |
| [`examples/teams`](examples/teams) | Organization membership, roles, and email invitations: multi-org `Membership`, a `require_role` guard, `#[mailer]` invite emails, idempotent accept, and role-gated member management |
| [`examples/media-room`](examples/media-room) | Live-media plugin: installs `autumn-media-plugin` with the rooms primitive and creates/lists mesh-call rooms through the mounted `RoomService` (see the [media guide](docs/guide/media.md)) |
| [`examples/invoice`](examples/invoice) | Renders one Maud view as both an on-screen detail page and a downloadable PDF via `autumn_web::pdf::Pdf` (see the [PDF downloads guide](docs/guide/pdf-downloads.md)) |

## Documentation

- [**What's new in 0.7.0**](docs/releases/0.7.0.md) — a walkthrough of the release: host-preparing deploys and fleets, deterministic simulation testing, the new model attributes, failure-capsule replay, and a request path that allocates ~59% less
- [Getting Started Guide](docs/guide/getting-started.md)
- [Authentication](docs/guide/authentication.md) — sessions, password policy, login/logout, `#[secured]`, lockout, and remember-me; the hub that links OAuth, step-up, and MFA
- [Dev-Loop Latency Budget](docs/guide/dev-loop-latency.md) — p50/p95/max budgets per change class, measurement methodology, and CI gates for `autumn dev`
- [Cache Coherence](docs/guide/cache-coherence.md) — `autumn cache audit`: the build fails when a `#[repository]` write can leave a `#[cached]` read stale with no invalidation covering it, turning cache invalidation from a runtime footgun into a compile-time obligation
- [Data Classification](docs/guide/data-classification.md) — `#[classified]`: a personal-data column is carried as a taint on the *type*, so returning it from a JSON response without passing a declared declassification boundary is a compile error, and `autumn data-flow` emits the diffable manifest of which sinks each classified field can reach
- [Compile-Time Query Budgets](docs/guide/query-budgets.md) — `#[query_budget(N)]`: the build fails when a handler's reachable paths can exceed its declared query count, catching N+1 regressions on every branch instead of only the ones a test exercises
- [The Agent Authority Envelope](docs/guide/agent-authority.md) — `#[agent_operable(grant = ...)]`: an agent-callable handler's blast radius becomes a compile-time constant, so a write, outbound host, webhook, job or cross-tenant query the declared grant does not allow fails the build, `autumn agents manifest --check` keeps the diffable record (including MCP tools nothing governs), and every `tools/call` is audited against what the compiler proved
- [The Architecture Graph](docs/guide/architecture-graph.md) — `autumn graph impact Post`: the framework derives a typed graph of the application from the macros that declare it (routes, models, repositories, jobs, plus each route's auth requirement and the tables it touches) and embeds it in the binary, so impact analysis is a query rather than a full-codebase read, `/actuator/graph` answers from the running process, and `autumn graph show --check` fails the build when a declared element or an edge quietly disappears
- [Signed Webhook Intake](docs/guide/signed-webhooks.md)
- [Platform Support](docs/guide/platform-support.md) — the Windows tier policy: which commands run natively, which need WSL2, and the `windows-latest` CI job that gates the native journey
- [Docs Smoke Procedure](docs/guide/docs-smoke.md) - release gate for first-run docs
- [Release Checklist](docs/release-checklist.md)
- [Code Generators](docs/guide/generators.md) — `autumn generate model | migration | scaffold`
- [Data Playground](docs/guide/console.md) — `autumn console`, the pre-wired edit-and-run answer to `rails console`
- [One-Off Tasks](docs/guide/tasks.md) - `#[task]`, `one_off_tasks![]`, and `autumn task`
- [Embedded Clustering](docs/guide/clustering.md) — zero-dependency two-node clustering: `[cluster]` config, authenticated gossip membership, the cluster-wide CRDT counter via `ClusterHandle`, and the `cluster:membership` health indicator
- [Multi-Replica Scheduled Tasks](docs/guide/scheduled-multi-replica.md) - `#[scheduled]` with Postgres advisory-lock coordination
- [Fleet Deploys](docs/guide/fleet-deploys.md) — `[deploy] hosts`: `autumn deploy up` rolls a release across several VPS hosts one at a time (per-host blue/green, migrations exactly once, halt-and-roll-back on failure), plus `deploy status` drift detection, fleet maintenance, and the load-balancer contract
- [Data-Retention Sweeps](docs/guide/retention-sweeps.md) — `retention(...)` on `#[repository(...)]`: batched, soft-delete-aware, fleet-coordinated auto-purge, plus `autumn retention --dry-run`
- [Data Retention for Framework-Owned Data](docs/guide/data-retention.md) — one `[retention]` section that bounds every table Autumn creates (job history, tracking, idempotency, experiment assignments, webhook replay, sessions, audit archives), enforced by an in-process fleet-coordinated sweep, GDPR legal-hold aware, with `autumn db retention --dry-run`
- [Data Scrubbing](docs/guide/data-scrubbing.md) — `autumn db scrub`: turn a production backup into an anonymized staging copy, with fail-closed PII classification driven by `#[encrypted]` columns, GDPR anonymize registrations, and a checked-in `scrub.toml`
- [Horizontal Sharding](docs/guide/sharding.md) — `[[database.shards]]`, slot-based routing, `ShardedDb`/`Shards` extractors, per-shard health and migrations
- [Per-Tenant Memory Cells](docs/guide/tenant-cells.md) — `TenantCell` byte accounting with the `tenancy.quota_bytes` soft quota and deterministic per-tenant eviction
- [Operating Background Jobs](docs/guide/operating-background-jobs.md) - admin dashboard and recovery actions for `#[job]`
- [OpenAPI Spec Generation](docs/guide/openapi.md) — the spec Autumn derives from your handlers, `#[api_doc(...)]`, `#[derive(OpenApiSchema)]`, Swagger UI, and the production profile gate
- [Exposing Your API as MCP Tools](docs/guide/mcp.md) — project typed endpoints into a Model Context Protocol server with `#[api_doc(mcp)]` + `mount_mcp`
- [Mail Guide](docs/guide/mail.md)
- [Widget Stories](docs/guide/stories.md) — the `/_stories` widget gallery and the `story!` macro
- [View Formatting Helpers](docs/guide/format-helpers.md) — `number_to_currency`, `pluralize`, `truncate`, `time_ago_in_words`, and friends for Maud templates
- [Cloud-Native Guide](docs/guide/cloud-native.md)
- [Capacity Contracts](docs/guide/capacity-contracts.md)
- [Logging & PII](docs/guide/logging-pii.md)
- [Failure Capsules](docs/guide/failure-capsules.md) — `[failure_capture]` records a failing request, its database traffic and its clock reads as one replayable file; `autumn replay` re-runs it offline
- [Edge Capsules](docs/guide/edge.md) — `#[edge]` compiles read-path routes into a portable `wasm32-wasip1` artifact a CDN can run, byte-identical to the origin and falling back to it for anything the edge cannot serve (experimental)
- [Sandboxed Plugins](docs/guide/sandboxed-plugins.md) — install an unaudited third-party plugin as a capability-sandboxed `wasm32-wasip1` artifact: one declared prefix, no filesystem/network/env/database, hard CPU and memory ceilings, and a trap that is a 5xx on its own prefix instead of a dead process (experimental)
- [Todo Tutorial](docs/guide/tutorial/index.md)
- [Autumn Harvest Architecture Notes](docs/autumn-workflow-architecture.md)
- [API Reference](https://docs.rs/autumn-web)
- [Pre-rendering Design Notes](docs/design/hybrid-rendering.md)
- [Stability Policy](STABILITY.md) — SemVer, MSRV, and migration commitments
- [Upgrading](docs/guide/upgrading.md) — `autumn upgrade` applies each release's mechanical API migrations to your own code **and** reconciles your project's framework-owned scaffold files (`Dockerfile`, `build.rs`, CI workflow, toolchain configs) against the current release, after showing you the diff; files you edited are flagged as conflicts rather than overwritten, `--check` exits nonzero on drift for CI, and everything it cannot safely rewrite is listed with `file:line` and a guide link
- [Transition effects](docs/guide/transition-effects.md) — per-edge `on` / `on_commit` side effects on `#[state_machine]` transitions.
- [Counter Caches](docs/guide/counter-cache.md) — `#[belongs_to(Post, counter_cache)]` keeps `posts.comment_count` current atomically, in the same transaction, with an idempotent `recompute` for drift
- [Maintained Derived Read Models](docs/guide/derivations.md) — `#[derivation(Post, column = "published_comment_count", filter = published)]` keeps a filtered count or weighted `sum(field)` on the parent current in the same transaction, with a content-addressed definition hash, a resumable backfill and `/actuator/derivations` for state and drift
- [Votes, Likes and Reactions](docs/guide/votable.md) — `#[votable(by = ..., aggregate = sum|count)]`, the race-safe `react()` / `reaction_of()` helpers, and the no-JS `reaction_controls` widget
- [SEO](docs/guide/seo.md) — `seo(...)` on any route macro plus the `SeoMeta` extractor, canonical URLs, `robots = "noindex"` and its sitemap exclusion, `SitemapSource`, and the auto-mounted `/sitemap.xml` + `/robots.txt`
- [Forms, Validation and Normalization](docs/guide/forms.md) — the `ChangesetForm` round-trip that re-renders a rejected submission with the user's input and inline errors, `Valid<T>` / `Validated<T>` for API endpoints, model-level `#[validate(...)]`, `#[normalize(trim, downcase, …)]` and where it runs on the write path, htmx inline validation and its automatic no-JavaScript fallback
- [Extractors](docs/guide/extractors.md) — the full extractor catalog, the two ordering rules, writing your own, and `Query<T>`'s structured decoding: repeated keys, `tags[]`, `tags[0]`, `filter[status]` and `items[0][sku]` all decode into typed sequences and nested structs
- [Cookie Consent](docs/guide/cookie-consent.md) — the `Consent` gate that is the actual compliance (not the banner), `inject_consent_banner`, the strictly-necessary exemption, policy-version re-prompting, and the GDPR Art. 7(3) withdraw flow
- [Threaded Comments on Anything](docs/guide/commentable.md) — `#[commentable]`, the polymorphic `(commentable_type, commentable_id)` association, `add_comment()` / `comment_thread()` / `delete_comment()`, the registry-driven comment router, and the no-JS `comment_thread` widget

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

## Platform support

Develop on **macOS, Linux, or Windows**; deploy on **Linux**.

On Windows the core journey — `autumn new`, `doctor`, `setup`, `dev`, `test`,
foreground `serve`, and managed Postgres — is **Tier 1: works natively**, and a
`windows-latest` CI job walks that whole journey on every pull request. The
Unix-native slices — the `autumn serve --daemon` lifecycle, the `autumn deploy`
actions that reach a host over SSH, and the bash contributor gate scripts — are
**Tier 2: supported via WSL2**, and fail
fast on native Windows with an error naming the policy rather than
half-working.

See [Platform support](docs/guide/platform-support.md) for the full
command-by-command policy, the Windows prerequisites `autumn doctor` flags, and
how to run the Tier 2 commands under WSL2.

## License

MIT OR Apache-2.0

