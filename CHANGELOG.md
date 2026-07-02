# Changelog

All notable changes to the Autumn framework will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **model:** many-to-many associations via `#[has_many(Target, through =
  join_table)]` (#1324). Extends the `belongs_to`/`has_many`/`has_one`
  associations from #835 with a join-table variant: join columns default to
  `{source}_id` / `{target}_id` (overridable with `fk = ...` / `target_fk =
  ...`), and `#[model]` emits the join table's `diesel::table!` itself — no
  hand-written `schema.rs` entry needed, only a migration creating the join
  table with a composite primary key on both columns. Participates in the
  existing `{Model}Preload` builder and `Preloadable` machinery: `preload(&[
  ...])` issues one batched `INNER JOIN` query per association level (fixed
  query count regardless of result-set size), and un-preloaded access yields
  the typed `NotLoaded` state. Nested preload paths work through a join
  (`Post::preload().tags_with(Tag::preload().posts())`). The generated
  `#[repository]` gets three mutation helpers per association —
  `add_{singular}`, `remove_{singular}`, `set_{plural}` (replace-all) — each
  idempotent (`add`/`set` use `ON CONFLICT DO NOTHING`) and, for `set_*`,
  wrapped in a single transaction. `examples/reddit-clone` demonstrates a
  real Post↔Tag many-to-many with preload and mutation usage in
  `src/routes/posts.rs`. Purely additive extension of the `#[has_many]`
  syntax; existing `belongs_to`/`has_many`/`has_one` usage is unaffected;
  minor version bump.
- **cache:** read-through fills with single-flight cache stampede protection
  (#1204). `cache::get_or_compute(cache, key, ttl, fill)` computes a missing
  value once per process for concurrent callers — the first miss becomes the
  "leader" and runs `fill`; every other concurrent caller for the same key
  awaits that one fill via a process-global in-flight registry instead of
  recomputing it. `cache::get_or_compute_with(cache, key, options, fill)`
  adds opt-in cross-replica protection: `GetOrComputeOptions::
  distributed_fill_lock(true)` acquires a Redis `SET NX PX` lock (with a Lua
  compare-and-delete release) so N replicas don't refill the same key N
  times, and `GetOrComputeOptions::stale_while_revalidate(grace)` serves the
  last-known value immediately while at most one background refresh runs. A
  failing fill never poisons the key: the leader gets a typed
  `CacheFillError::Fill`, waiters get a rendered `CacheFillError::FillFailed`,
  nothing is written to the cache, and the next caller retries; the
  distributed lock's TTL bounds the damage from a crashed filler.
  `cache::jittered_ttl(base, fraction)` de-synchronizes mass expiry of keys
  written together. New `autumn_cache_read_through_*` and
  `autumn_cache_fill_lock_*` counters are exposed on both
  `/actuator/metrics` and `/actuator/prometheus`. Additive: the `Cache` trait
  gains two default methods (`try_acquire_fill_lock`/`release_fill_lock`,
  default `Unsupported`/no-op) so existing backends keep working unchanged.
  See the new [Cache Stampede Protection guide](docs/guide/cache-stampede.md).
  Hardening from review: SWR freshness checks now correctly distinguish a
  stale envelope from a completed refresh in the lock-poll path, `ttl: None`
  combined with `stale_while_revalidate` no longer serves as permanently
  stale, the lock-poll loop backs off exponentially instead of polling at a
  fixed cadence, background refreshes are capped at 64 concurrent across all
  keys, the Redis fill-lock key lives in its own namespace instead of one
  that could collide with an ordinary cache key, and SWR freshness is now
  evaluated through the same injectable `ClockSource` used elsewhere in the
  framework rather than a hard-coded `SystemTime::now()`.
- **views:** `link_to`/`button_to` view helpers for safe, method-aware links
  (#1138). `link_to`/`link_to_with` render an HTML-escaped GET `<a>` anchor
  with optional `class`/`target`/`rel`/extra attributes (e.g. htmx `hx-*`),
  automatically adding `rel="noopener"` when `target="_blank"` is set.
  `button_to`/`button_to_with` render a single-button `<form>` for
  state-changing actions: the CSRF token is a **required** argument, so a
  `button_to` call cannot compile without one in scope. For any method other
  than `GET`, the form posts and carries a hidden `_method` override (reusing
  `form::method_input`) plus the hidden CSRF field; `Method::GET` renders a
  plain GET form with neither. Both helpers accept extra attributes on an
  options struct, so `button_to_with("Delete", path, Method::DELETE, token,
  &ButtonToOptions::new().attrs(&[("hx-delete", path)]))` upgrades to an
  htmx interaction without hand-rolling the form. The admin plugin's
  hand-rolled delete button now uses `button_to_with`, gaining a working
  no-JS fallback (POST + `_method=DELETE` + CSRF) for free. Purely additive;
  minor version bump.

- **jobs:** tracked job handles with progress reporting and a built-in
  pollable status route (#1373). `job::enqueue_tracked`/`enqueue_tracked_for`
  (and the generated `{Job}::enqueue_tracked`/`enqueue_tracked_for`
  companions) return a `TrackedJobHandle` carrying a public, unguessable
  token distinct from the internal job id. `#[job]` accepts an optional third
  `JobContext` argument (`async fn(AppState, Args, JobContext)`) so a handler
  can call `ctx.set_progress(pct, message)`, `ctx.set_result(json)`, and
  `ctx.set_user_error(message)`; only the final failed attempt (or a panic)
  settles the tracked record, so progress survives retries. A new
  `GET /_autumn/jobs/{token}` route (on by default; opt out via
  `jobs.tracking.route_enabled = false`) returns the status as JSON for API
  clients or a self-polling htmx fragment (`hx-trigger="every 2s"`) for
  browsers, dropping the poll trigger once the job reaches a terminal state.
  Tokens can be bound to a session/user via `TrackedJobOwner`/
  `TrackedJobOwner::from_session`; a mismatched or unknown token gets an
  identical 404 (no enumeration/ownership oracle). Records expire after a
  configurable TTL (`jobs.tracking.ttl_secs`, default 24h) and are persisted
  by whichever job backend is already configured — in-memory (`local`),
  Redis (`redis`), or the new `autumn_job_tracking` table (`postgres`, via a
  bundled framework migration) — so tracked jobs need no separate setup.
  Purely additive: existing `#[job]` handlers and `enqueue` calls are
  unaffected; minor version bump.

- **cli:** `references` field type for `autumn generate model`/`scaffold`
  (#1026). `post:references` (or `references:`, either casing) scaffolds a
  foreign key: the field name resolves to `post_id`, the referenced table is
  derived via the existing `naming::pluralize` (`Post` -> `posts`), and the
  migration emits `post_id BIGINT NOT NULL REFERENCES posts(id)` plus an
  automatic index (`idx_<table>_post_id`) — no `--index` flag needed. Append
  `?` for a nullable FK (`post:references?` -> `post_id: Option<i64>`). The
  generated `#[model]` field is `post_id: i64` (`Int8` in `schema.rs`), and
  `down.sql` reverses cleanly (`DROP TABLE`, or an implicit column-owned
  index/constraint drop for the `ALTER TABLE ADD COLUMN` migration shape). An
  unknown referenced model (no `src/models/<base>.rs` or matching
  `src/models.rs` entry) still scaffolds, with a warning that the table is
  assumed to already exist; a referenced model that *is* found but declares a
  UUID primary key fails fast with a clear error instead of emitting a
  `BIGINT`-vs-`UUID` foreign key that would break at `autumn migrate` time.
  `generate migration Add…To…` gets the same warning/error behavior as
  `generate model`/`scaffold` for consistency. `references` is listed in
  `SUPPORTED_TYPES` / `--help`, and the generated scaffold smoke test creates
  a minimal stand-in for the referenced table (skipping self-referential FKs,
  which target the table already being created) so it stays runnable
  standalone.
- **auth:** scoped service tokens whose scopes flow into policy checks (#1158).
  Mint named, optionally-expiring API tokens carrying a set of flat scopes
  (e.g. `posts:read`) via `IssueTokenSpec` + `issue_scoped_api_token`; tokens
  stay hashed at rest. The `ApiTokenStore` trait gains additive,
  default-implemented `issue_scoped` / `verify_scoped` / `list` / `rotate`
  methods (existing impls keep compiling), and the built-in `InMemoryApiTokenStore`
  / `DbApiTokenStore` record `last_used_at` and reject expired tokens (401).
  `PolicyContext` gains `has_scope` / `has_any_scope` / `has_all_scopes`
  mirroring the role accessors, populated from the authenticating token via the
  new `ApiTokenScopes` request extension and `authorize_with_scopes` /
  `PolicyContext::from_request_parts`. `#[secured(scopes = ["posts:write"])]`
  gates a handler on token scopes (default-deny, `403` when missing) and works
  for pure service principals with no session; `#[secured("admin", scopes = […])]`
  requires both. Management surface: helper API, `autumn token`
  (`issue --name/--scope/--expires-at`, `list`, `rotate`), and an
  `autumn-admin-plugin` `TokenAdminModel` panel. Additive `api_tokens` columns
  (`name`, `scopes` JSONB, `expires_at`, `last_used_at`) via a new framework
  migration; minor version bump, no breaking change to `autumn-web`.
- **cli:** `autumn generate tauri` — scaffolds a complete `src-tauri/` sidecar
  project so any existing autumn app ships as a native desktop installer with a
  single additional command (`cargo tauri build`). Uses the Tauri v2 sidecar
  model: the autumn server binary is supervised by the Tauri shell and the
  webview loads from an ephemeral loopback port chosen at runtime. Fully
  self-contained at runtime via managed local Postgres (#1119,
  `managed-pg-bundled`) and single-binary asset embedding (#1004,
  `embed-assets`). Generator is purely additive — never rewrites your app's
  `src/main.rs` or root `Cargo.toml`. Idempotent, dry-run capable, prints
  required external prerequisites after scaffolding (#1150).
- **ui:** reusable Maud pagination-nav renderer — `autumn_web::ui::pagination::{pagination_nav, cursor_pagination_nav, PagerOptions}`,
  re-exported from the prelude. Renders an accessible (`<nav>` with
  `aria-current="page"` and non-focusable disabled prev/next), filter-preserving
  (keeps the current query string, swapping only the `page`/`size` params),
  htmx-opt-in, windowed pager (`1 … 4 5 6 … 20`) from an existing `Page`, plus a
  cursor variant for `CursorPage` feeds. The admin plugin's two hand-rolled
  pagers (`render_pagination`, `jobs_pagination`) now call the shared helper,
  removing the duplicated page-window logic (#1007).
- **ci:** feature-combination compile gate covering 35 `autumn-web` feature
  combinations — each individual flag in isolation (`cargo hack --each-feature`)
  plus curated real-world combos (`db`, `mail`, `maud,htmx`, `storage,db`,
  `telemetry-otlp`) — so downstream apps building with a trimmed feature set
  can't silently break between releases (#982).

## [0.6.0] - 2026-06-30

### Added

- **ui:** reusable `card` and `stat_card` Maud widget helpers in
  `autumn_web::widgets`, re-exported from the prelude. `card()` renders a
  titled content panel with an optional header-action slot, footer, and
  configurable heading level (`HeadingLevel::H1`–`H6`, default `H2`);
  `stat_card()` renders a metric tile with label, value, and optional link.
  Both are CSP-safe and HTML-escape caller-supplied text via Maud.
  `CardConfig` uses a builder pattern with `const fn` and private fields
  so the `title()` / `title_html()` escape path cannot be bypassed.
  The admin plugin's 12 hand-rolled card blocks and dashboard stat tiles are
  migrated to the new helpers, removing the duplication (#1122).

## [0.5.0] - 2026-06-16

### Added


- **daemon:** `autumn serve` — run an app as a production (non-watch) local
  daemon, with an optional managed local Postgres (#1119)
  - `autumn serve` runs the compiled app in the foreground as a production
    server (distinct from `autumn dev`: no file watching or hot reload).
    `--release` builds an optimized binary.
  - `autumn serve --daemon` backgrounds the server under a PID lockfile (a
    second start is rejected with a clear message instead of double-binding);
    `autumn serve stop | status | restart` manage its lifecycle. Graceful
    shutdown reuses the existing lame-duck drain via `SIGTERM`.
  - The server binds a **Unix domain socket** (new `server.unix_socket` /
    `AUTUMN_SERVER__UNIX_SOCKET`) — never a public interface by default — and
    the chosen address is written to a discovery file for clients. PID, socket,
    address file, and logs live under platform dirs (XDG / `%APPDATA%`), never
    cwd or `/etc`.
  - `autumn new --daemon` scaffolds a model-free starter that builds with **no
    Postgres** (drops the `db` feature and migrations) — runnable as a daemon
    with zero external dependencies.
  - `ManagedPostgresPoolProvider` (feature `managed-pg`) provisions and
    supervises a local Postgres in the app's data dir through the existing
    `with_pool_provider` seam (no query-path changes); `managed-pg-bundled`
    embeds the Postgres binaries in the app executable. `autumn new
    --bundled-pg` scaffolds and wires it.

- **sharding:** `from_shard(db: &ShardedDb) -> Self` constructor on generated
  repositories (#1273)
  - `#[repository]` now emits `from_shard` as the standard way to build a
    repository over a shard while preserving full request instrumentation:
    statement timeout, slow-query threshold, and shard-tagged route metric
    label are all carried from the `ShardedDb` context rather than reset to
    framework defaults.
  - The previous `with_pool` constructor is **renamed** to
    `with_pool_untracked` to signal at the call site that request
    observability is bypassed. Uses of `with_pool` on generated repositories
    must be updated to `with_pool_untracked` (only the name changes; the
    signature and semantics are identical).
  - `ShardedDb` gains a `#[doc(hidden)]` `__autumn_repository_seed()` accessor
    exposing the `ShardRepositorySeed` carrier struct used by generated code.

- **middleware:** `AppBuilder::static_gate` — auth gating for SSG/ISG routes
  via a pre-static middleware hook (#848)
  - Cached SSG/ISG pages are served by the static-first middleware before the
    inner router (session, auth) is reached, so framework auth layers could not
    gate pre-rendered responses. `static_gate` registers a Tower layer that runs
    **outermost** — outside the session layer and ahead of the static cache —
    so it can redirect or reject a request before a cached page is served
    (Autumn's analogue of Next.js Edge Middleware).
  - Runs in the same outermost position in both SSG/ISG and fully-dynamic
    modes, so gating code is portable. Has access to request headers/cookies but
    **not** the session `Extension` (verify a signed/JWT cookie directly).
  - Plugin pre-flight helpers `has_static_gate::<L>()` /
    `get_static_gate_types()`, and a matching `TestApp::static_gate` for tests.
  - Additive only; documented in `docs/guide/middleware.md`.
- **db:** Declarative associations and eager loading for `#[model]` / `#[repository]` (#835)
  - `#[model]` accepts struct-level `#[belongs_to(Target, fk = ...)]`,
    `#[has_many(Target, fk = ...)]`, and `#[has_one(Target, fk = ...)]`.
    Foreign keys are inferred by convention (`belongs_to` → `{target}_id` on
    this model; `has_many`/`has_one` → `{source}_id` on the target) and
    overridable with `fk = …`. The accessor/store name is derived by
    convention and overridable with `name = …`, so multiple associations can
    target the same model (e.g. `authored` / `approved` both → `Post`) without
    colliding. The schema and association set live in one place — no per-pair
    `Related` impl.
  - Codegen emits a `{Model}Preload` spec builder (`Model::preload()`), a
    `{Model}Associations` accessor trait implemented for `Preloaded<Model>`,
    and a `Preloadable` impl that issues the batched queries.
  - `#[repository]` gains `preload(records, spec)` returning
    `Vec<Preloaded<Model>>`. It issues **at most one** `WHERE ... IN (...)`
    statement per association per level (`belongs_to`/`has_one` keyed on the
    parent/target id; `has_many` grouped client-side), with **no** per-row
    fetches and **no** implicit lazy loading. Nested paths are supported, e.g.
    `Post::preload().author().comments_with(Comment::preload().author())`.
  - New `autumn_web::preload` module: `Preloaded<T>` (derefs to the record),
    the type-erased `Associations` store, the typed `NotLoaded` accessor error
    (accessing an un-preloaded association is an error, never SQL), the
    `Preloadable` trait, and the `impl_preloadable_leaf!` macro for
    hand-written association targets.
  - Preload SQL runs on the **same read role** as the parent finder (the
    repository's snapshotted `ReadRoute`); `on_primary()` pins the whole chain.
    With `CursorPage`, preloads execute **after** the overfetch/truncate.
  - Preloaded associations honor the target's **read scoping**, keyed off the
    target's `#[repository]` config (not field presence): when the target
    repository is `soft_delete`, soft-deleted rows (`deleted_at IS NOT NULL`)
    are hidden; when it is `tenant_scoped`, rows outside the ambient
    `CURRENT_TENANT` are hidden — mirroring the target's finders. A
    `deleted_at`/`tenant_id` column on a model whose repository does *not* opt
    in is left unfiltered. `repo.across_tenants().preload(...)` skips the
    tenant predicate at every level, matching `across_tenants()` finders.
  - `examples/reddit-clone` migrated: the front page and single-post view drop
    their hand-written joins / per-row author lookups for `preload`. See
    `docs/adr/0008-associations-and-eager-loading.md`.
- **db:** Framework-native horizontal sharding (`[[database.shards]]`)
  - Tenant data routes key → logical slot (fixed at 16384 slots, matching
    Redis Cluster/Valkey — nothing to choose or outgrow; deterministic
    FNV-1a/splitmix64 hash pinned by golden-vector tests) → physical shard
    per an explicit `slots` map, so resharding moves whole slots in config
    instead of rehashing keys. Each shard is a full primary/replica
    `DatabaseTopology` with per-shard `replica_fallback`.
  - New `autumn_web::sharding` module and prelude extractors: `ShardedDb`
    (tenant-routed via `ShardKeyOverride` → tenancy task-local → tenant
    extraction; derefs like `Db` with the same `tx` semantics) and `Shards`
    (`db_for`/`read_for`/`db_on` plus a bounded concurrent `each_shard`
    fan-out that collects per-shard results — there are no cross-shard
    transactions). Pluggable `ShardRouter` via
    `AppBuilder::with_shard_router`; per-shard pool decoration via
    `DatabasePoolProvider::create_shard_topology`. `#[repository]` gains a
    `with_pool` constructor for shard-scoped repositories.
  - Startup auto-migrate and `autumn migrate` apply migrations control-first
    then per shard, fail-fast with target labels; new `--shard <name>` /
    `--control-only` flags and per-target `status`. Per-shard replica
    migration parity gates each shard's replica reads.
  - `/ready` and `/actuator/health` gain `db:shard:<name>` components;
    `/actuator/metrics` gains a `database_shards` block; shard-routed
    checkouts tag spans (`db.shard`) and route metrics (`shard=<name>`).
  - Framework state (jobs, scheduler locks, sessions, flags) stays on the
    unsharded control role — enforced at config validation. New
    `examples/bookmarks-sharded` Docker Compose stack and
    `docs/guide/sharding.md`.

- **auth:** Active session management with device list and revocation in the auth starter (#819)
  - `autumn generate auth` now persists a `{user}_sessions` row per login
    (SHA-256 digest of the opaque session id — never the raw id — plus user id,
    IP at login, raw + parsed User-Agent, optional device label, `created_at`,
    `last_seen_at`), created on password login, email confirmation, TOTP verify,
    and passkey login, and removed on logout.
  - Generated handler APIs on the user model: `sessions()`, `revoke_session(id)`,
    `revoke_other_sessions(current_digest)`, and `revoke_all_sessions()`, plus a
    `require_tracked_session` gate used by every generated authenticated route.
    The row is the source of truth: revoking it makes the device's **next**
    request 401 (the cookie session is destroyed too), with no reliance on
    cookie expiry. `last_seen_at` writes are throttled to at most one per
    `[auth.sessions].last_seen_update_secs` (default 60 s) per session.
  - New `/account/sessions` Maud + htmx page: per-session revoke buttons,
    device labels, and a one-click "Sign out everywhere else".
  - Credential-changing events — password reset, TOTP enrollment/disable, and
    passkey add/remove — revoke all *other* sessions by default, configurable
    via the new `[auth.sessions] revoke_on_credential_change` flag (default on).
  - New `autumn_web::user_agent` module: a dependency-free heuristic
    `parse_user_agent` (browser family / OS / device class) with a documented
    one-line swap point for custom parsers.
  - Generated `tests/auth_sessions.rs` covers the two-client flow (log in twice,
    revoke from one client, the other's replayed cookie 401s) and generated
    `docs/guide/session-management.md` documents the APIs, the privacy posture
    for stored IP/UA (purpose limitation, retention scrubbing SQL, IP
    truncation), and the migration path for existing auth-starter apps.
  - Additive only: one new table in the auth-starter migration; no public API
    removed.
- **jobs:** Job uniqueness keys and concurrency limits for `#[job]` (#829)
  - `#[job(unique)]` dedupes enqueues on a stable hash of the full args;
    `unique_by = "field, …"` derives the key from selected args fields. The
    uniqueness window is configurable: `unique_window = "running"` (default:
    held while pending or running), `"pending"` (released when execution
    starts), or `unique_for_ms = N` (TTL debounce from enqueue time). A
    coalesced enqueue is a no-op `Ok(())` — N identical enqueues in a burst
    execute exactly once.
  - `#[job(concurrency = N)]` caps simultaneously-executing jobs of the type;
    `concurrency_key = "field"` scopes the cap per distinct args value
    (e.g. at most one `recalculate_account` per account). Excess jobs wait
    for a slot rather than running or being dropped.
  - Enforced consistently on all three backends and distributed-safe on the
    durable ones: Postgres uses an additive schema (nullable columns + a
    partial unique index with `ON CONFLICT DO NOTHING`) and concurrency-aware
    claims serialized by a transaction-scoped advisory lock only when a
    limited job is registered; Redis uses `SET NX PX` unique locks and atomic
    Lua claim/settle scripts with a parked-jobs zset.
  - Keys and slots are released on success, terminal failure, and worker
    crash (visibility-timeout recovery / TTL backstop), so a dead worker
    cannot deadlock a unique key or leak a concurrency slot. Retries keep
    the key held but free the slot during backoff.
  - Observability: `/actuator/jobs` adds `total_deduplicated` and
    `blocked_on_concurrency` per job, and the job admin model gains the
    `deduplicated` status.
  - Additive and non-breaking: jobs without the new attributes behave
    exactly as before; the `autumn_jobs` schema change is additive; minor
    version bump.

- **log:** Structured per-request access log, on by default (#999)
  - Every served HTTP request now emits **exactly one** structured access-log
    event (`tracing` target `autumn::access`, level `INFO`) at the response
    boundary, carrying `method`, `route` (the matched low-cardinality template,
    e.g. `/users/{id}` — never the raw path), `status`, `duration_ms`, and the
    `request_id` that matches the `x-request-id` header and error pages.
  - Dual placement: the **primary** layer emits inside the request
    span/log context (correlated, request id from the request extension) and
    marks the response; an **outermost fallback** at the router assembly
    boundary logs only responses the primary never saw — startup 503s,
    pre-built static (SSG/ISR) page hits, session-store outage 503s, and
    requests to the late-mounted MCP endpoint — with the wire status and no
    request id (those paths never run `RequestIdLayer`).
  - Rendered by the standard subscriber, so it honors `log.format`: a readable
    line under `pretty`, a single JSON object per line under `json`. Works with
    **no** `telemetry-otlp` feature and no OTLP collector — operators on
    `docker logs` / platform log drains get request-level visibility for free.
  - Steady-state probe/asset noise is excluded by default (`/health`,
    `/live`, `/ready`, `/startup`, `/actuator/*`, `/static/*`); the set is
    configurable via `log.access_log_exclude` (whole-segment prefix matching)
    or `AUTUMN_LOG__ACCESS_LOG_EXCLUDE` (comma-separated). Unmatched requests
    log the low-cardinality `_unmatched` route label.
  - On by default; turn off with `log.access_log = false` in `autumn.toml`
    or `AUTUMN_LOG__ACCESS_LOG=false` — no recompile needed.
  - The line never includes query strings, headers, or bodies, preserving the
    log-scrubbing posture established for logs (#697) by construction.
  - Additive `LogConfig` fields only (`access_log`, `access_log_exclude`);
    non-breaking, minor version bump.

- **log:** Request-scoped log context that auto-tags every log line (#1169)
  - An always-on `LogContextLayer` establishes a fresh `tokio::task_local`
    `log::context::LogContext` for **every** HTTP request, seeded with the same
    `request_id` used by the `x-request-id` header and error pages. It is not
    gated behind `telemetry-otlp` and is applied inner to `RequestIdLayer` so the
    request id is always available.
  - The request is driven inside a `tracing` span carrying
    `request_id`/`user_id`/`tenant_id`, so every `tracing` event emitted during
    the request automatically correlates back to it — no manual field threading.
  - When the request authenticates, `user_id` is added to the context
    automatically (from both the `#[secured]` session check and the `RequireAuth`
    middleware); when multi-tenancy resolves a tenant, `tenant_id` is added
    automatically (from the tenancy middleware).
  - Handler/service code can attach custom fields with
    `autumn_web::log::context::with_log_field("order_id", id)` (re-exported from
    the prelude). The well-known ids (`request_id`/`user_id`/`tenant_id`) ride the
    request span and render in ordinary `tracing` output; custom fields are
    carried in the context for **structured** consumers — the actuator log buffer
    (#1168), the access line (#999), or any context-aware layer — rather than the
    default stdout formatter. Reserved keys cannot be shadowed by custom fields.
  - The context stays active while a streaming/SSE response body is produced (the
    body is re-scoped per frame, mirroring tenancy), and synchronous work in a
    downstream layer's `Service::call` is correlated too.
  - Context is isolated per request (nothing leaks across requests) and a
    `tokio::spawn`'d task does **not** inherit it unless explicitly propagated via
    `log::context::in_current_context(..)`, which re-enters the request span too.
  - Sensitive custom-field values are scrubbed through the existing
    `log/filter.rs` key filter (#697), so secrets never enter the context output.
  - Additive, non-breaking surface (minor version bump). Establishes the
    correlating primitive consumed by the per-request access line (#999) and the
    actuator log-view buffer (#1168).
- **mcp:** Expose typed endpoints as Model Context Protocol (MCP) tools so AI agents can call your API (#1117)
  - New `mcp` Cargo feature (implies `openapi`). `AppBuilder::mount_mcp("/mcp")` serves a spec-compliant MCP endpoint over Streamable HTTP, handling `initialize`, `tools/list`, and `tools/call`.
  - Endpoints opt in per-route via `#[api_doc(mcp)]`; nothing is exposed implicitly. `#[api_doc(mcp = false)]` force-excludes a route.
  - A whole-API hatch, `AppBuilder::expose_all_as_mcp()`, auto-includes every eligible `GET`, but mutating verbs (`POST`/`PUT`/`PATCH`/`DELETE`) still require an explicit `#[api_doc(mcp)]` opt-in, and per-endpoint exclusions are always honored.
  - Each tool's `name`, `description`, and `inputSchema` are derived from the existing `ApiDoc` (operation id, summary/description, merged request-body + `Query` + path-param schemas) — there is no second, hand-maintained schema, so the tool catalog cannot drift from the handler's typed contract.
  - `tools/call` dispatches through the **real handler pipeline** (the same in-process path the test client uses), so `#[secured]`, authorization, tenancy, rate limits, and validation apply identically to an agent call and an HTTP call.
  - Agent authentication reuses the existing bearer-token surface (`RequireApiToken` / `ApiTokenStore`): the `Authorization`, `Cookie`, and `X-CSRF-Token` headers presented to `/mcp` are forwarded into the dispatched call, so bearer, session (`#[secured]`), and CSRF-protected routes behave identically to a direct request.
  - `Origin` validation (MCP Streamable-HTTP spec requirement) is enforced against the app's CORS `allowed_origins`: a browser `Origin` not in the allowlist gets `403`, while requests without an `Origin` (non-browser agents) pass — defending against DNS-rebinding without breaking agent clients.
  - `AppBuilder::secure_mcp(layer)` gates the entire `/mcp` endpoint (catalog included) behind any tower layer, e.g. `RequireApiToken`.
  - JSON-RPC robustness: rejects requests missing `jsonrpc: "2.0"`, empty/malformed batches, and non-object `arguments` with `-32600`/`-32602`; negotiates only supported protocol versions; enforces required `body` arguments; serializes array query fields with form/explode semantics; and reuses the framework path-segment encoder. Tool-result bodies are capped at 10 MiB. Duplicate tool names (same `operation_id`) keep the first registration with a build-time warning.
  - HTTP method maps to MCP safety annotations: `GET` → `readOnlyHint`; `DELETE` → `destructiveHint`.
  - Only JSON-in/JSON-out endpoints are eligible; HTML/Maud routes (no response schema) are auto-excluded with a build-time log note.
  - `examples/todo-app` gains an `/mcp` endpoint exposing `list_json` (read) and `create_json` (explicitly-opted-in write) behind `RequireApiToken`.

- **actuator:** Decouple the Prometheus scrape endpoint from sensitive mode (#857)
  - New `actuator.prometheus` config flag (default `true`) controls
    `/actuator/prometheus` **independently of** `actuator.sensitive`. Production
    apps can expose Prometheus metrics for platform scraping (e.g. Fly.io
    `[metrics]`) while keeping `sensitive = false`, so `/actuator/env`,
    `/actuator/configprops`, `/actuator/loggers`, `/actuator/tasks`,
    `/actuator/jobs`, and the actuator task UI stay off the public surface.
  - Set `actuator.prometheus = false` (or `AUTUMN_ACTUATOR__PROMETHEUS=false`)
    to remove the scrape endpoint entirely (it then returns `404`). The flag is
    surfaced in `/actuator/configprops`.
  - The `[actuator]` section now honors environment overrides
    (`AUTUMN_ACTUATOR__PREFIX`, `AUTUMN_ACTUATOR__SENSITIVE`,
    `AUTUMN_ACTUATOR__PROMETHEUS`), matching the documented
    `AUTUMN_SECTION__FIELD` convention. Previously the actuator section was only
    configurable via TOML.
  - Docs: `docs/guide/deployment.md` now describes the safe Fly.io deployment
    shape, including scraping a private/non-public metrics port, and clarifies
    that OTLP tracing and the Prometheus scrape endpoint are separate telemetry
    paths — enabling OTLP does not add OpenTelemetry metrics to
    `/actuator/prometheus` without an explicit bridge/exporter.
- **testing:** CSS-selector HTML assertions on `TestResponse` (#1147)
  - Autumn renders server-side HTML (Maud + htmx), so the in-process test client can now assert on page *structure* by CSS selector instead of brittle substrings. New chainable methods on `TestResponse`: `assert_selector(css)`, `assert_no_selector(css)`, `assert_selector_count(css, n)`, `assert_text(css, expected)`, `assert_text_contains(css, sub)`, and `assert_attr(css, attr, expected)`.
  - Non-asserting accessors for custom assertions: `selector_count(css) -> usize`, `selector_text(css) -> Vec<String>`, and `selector_attr(css, attr) -> Vec<Option<String>>` — each returns matches in document order.
  - Backed by a dependency-free HTML parser and CSS-selector matcher (`tag`, `.class`, `#id`, `[attr]`/`[attr=v]`/`[attr^=v]`/`[attr$=v]`/`[attr*=v]`, compound selectors, selector lists, and descendant/child combinators). Parses fragments literally, so bare `<tr>` htmx swaps are selectable — a spec HTML5 tree builder would foster-parent and drop them.
  - Assertions survive cosmetic template changes (whitespace, attribute order, wrapping markup) that break the equivalent `assert_body_contains` test. Failure messages print the selector, expected-vs-actual value, and a truncated outline of the parsed HTML.
  - Purely additive: no breaking change to existing assertions; no new published dependency. See the `autumn::test` module docs and `docs/guide/testing.md` for a worked example.

## [0.5.0] - 2026-06-04

### Added

- **dev inspector:** Built-in request inspector with N+1 query detection (#701)
  - In `dev` profile, `autumn-web` automatically mounts a request inspector UI at `/_autumn/inspect` (configurable via `[dev] inspector_path`). The route does not exist in `prod` or `test` profiles.
  - The inspector records the last N requests (default `N = 100`, configurable via `[dev] inspector_capacity`) in a bounded in-memory ring buffer. Each record includes HTTP method, path, status code, wall time, response Content-Type and Content-Length.
  - An N+1 detector flags any request that issued ≥ M structurally identical SQL statements (default `M = 5`, configurable via `[dev] inspector_n_plus_one_threshold`). The flag includes the offending SQL template and the repetition count.
  - A `RequestInspector` Axum extractor is available to handlers in `dev` profile to append SQL query records (with SQL text, bound parameters, elapsed time, and `file:line` call site). Integration tests can use the extractor to assert "this request issued exactly K queries."
  - The inspector UI (server-rendered HTML, no client-side framework) lists requests newest-first with method, path, status, duration, query count, and an N+1 warning badge. Clicking a request opens a detail view with a per-query timing table and a `curl` snippet to reproduce the request.
  - The inspector excludes its own requests from the ring buffer to avoid feedback loops.
  - New `[dev]` config section: `inspector_path`, `inspector_capacity`, `inspector_n_plus_one_threshold`.
  - Existing apps require zero changes — the inspector is purely additive.
  - See `docs/guide/dev-inspector.md` for the full guide.

- **pagination:** Wire first-class pagination into `#[repository]` and scaffold (#681)
  - `#[repository]` now generates a `page(req: &PageRequest) -> AutumnResult<Page<Model>>` method on every repository struct, enabling offset pagination without hand-written SQL.  Results are ordered by `id DESC` for deterministic page boundaries.
  - `#[repository(Model, cursor_key = field)]` additionally generates `cursor_page(req: &CursorRequest) -> AutumnResult<CursorPage<Model>>` — keyset pagination sorted by `(field DESC, id DESC)`.  The cursor payload encodes both the sort-key value and `id` so the keyset filter is always correct: `WHERE (field < after_k) OR (field = after_k AND id < after_id)`.
  - `autumn generate scaffold` index actions use the `PageRequest` extractor directly.  Out-of-range values are clamped silently (consistent with the framework rule that list endpoints never 400 for bad paging params).
  - Scaffold-generated routes include a `pagination_nav` Maud helper with htmx-friendly Previous / Next links.
  - `examples/todo-app` updated: `Todo::page` added; HTML list view uses `PageRequest` and renders pagination controls.
  - `docs/guide/pagination.md` added, covering: offset vs cursor decision guide, macro entry points, overriding page size, declaring a cursor key, htmx wiring.

To opt out of the generated `page` method: implement your own list handler using `repo.find_all()` or a custom Diesel query.  The `find_all` method is unchanged.

- **security:** Centralize trusted-proxies policy across forwarded-header middleware (#812)
  - **New `[security.trusted_proxies]` config block** at the top level of `[security]`.
    Configure once; every framework middleware (rate limiter, method-override origin check,
    CSRF, HSTS detection, tracing fields) honours the same trust boundary automatically.
    Fields: `ranges` (CIDR list), `trusted_hops` (peel-N-from-right strategy), and
    `trust_forwarded_headers` (global on/off switch). Profile-aware defaults: `dev` trusts
    loopback only; `prod` defaults to no forwarding trust until configured.
  - **New extractors** in `autumn_web::extract`: `ClientAddr` (resolved client IP),
    `ClientHost` (resolved external hostname), `ClientScheme` (`"http"` / `"https"` after
    `X-Forwarded-Proto` evaluation). These are the only blessed way to read client identity
    from handlers and middleware — direct `X-Forwarded-*` reads are now rejected by the
    new CI `grep` guard.
  - **Deprecation:** `security.rate_limit.trusted_proxies` and
    `security.rate_limit.trust_forwarded_headers` continue to work for one minor release
    with a deprecation warning at startup pointing at the new top-level config.
    `autumn doctor --strict` fails when both old and new are set with conflicting values.
  - **Regression fixes:** Closes three related CVEs — PR #753 (`X-Forwarded-For`
    rate-limit bypass), PR #785 and PR #791 (`X-Forwarded-Host` CSRF/method-override
    spoofing bypass in `MethodOverrideLayer`). The PoC from PR #791 is now covered by
    a regression test that validates the override is rejected when the
    `ResolvedClientIdentity` host does not match the `Origin` header.
  - **Plugin author guide** added to `docs/guide/middleware.md` and
    `docs/guide/extensibility.md`: "Never read `X-Forwarded-*` directly. Use
    `ClientAddr` / `ClientHost` / `ClientScheme` extractors."
- **configuration:** Add TOML config file support to generated scaffolds and a runtime configuration system for live-tunable operational knobs (#773, #931).
- **data and repositories:** Add soft delete, high-performance bulk CRUD, Postgres full-text search, automatic version history, CSV import/export, and per-query statement timeout/slow-query telemetry support (#858, #881, #905, #922, #1075, #865).
- **development loop:** Add the dev-mode error overlay, generator conformance CI gate, dev-loop latency budgets, and framework runtime benchmarks (#1080, #1079, #920, #756).
- **HTTP and routing:** Add safe HTML method override handling, ETag conditional GET helpers, per-request timeout and body-size middleware, first-class response compression, and API versioning with deprecation and sunset lifecycles (#605, #853, #996, #1083, #1077).
- **operations:** Add rolling-deploy shutdown contracts, maintenance mode middleware and CLI commands, W3C trace-context propagation across jobs/mailers, traced outbound HTTP client retries/mocks, outbound signed webhooks with retries/DLQ/actuator endpoints, and pluggable error reporting for panics and 5xx responses (#843, #917, #854, #863, #923, #1047).
- **security:** Add encrypted credentials, at-rest attribute encryption, direct browser-to-storage uploads, trusted-host validation, CSP nonces, log parameter scrubbing, per-principal/API-token rate limits, TOTP auth scaffolding, and WebAuthn passkey scaffolding (#849, #1058, #860, #885, #915, #903, #1001, #1057, #1070).
- **state and collaboration:** Add after-commit callbacks, HTTP idempotency-key middleware, row-level multi-tenancy, Redis-backed global rate limiting, first-class feature flags, A/B experiments, distributed presence, active search/autocomplete widgets, inline field validation, and an injectable `Clock` extractor for deterministic tests (#778, #779, #876, #764, #1000, #1016, #973, #989, #991, #1014).
- **content and tooling:** Add Markdown rendering with frontmatter/SSG support, `autumn generate mailer`, migration safety preflight checks, and plugin hooks at framework-owned dependency boundaries (#921, #866, #762, #862).
- Expose recent structured logs via GET /actuator/logfile (#1168, #1184).
- **cli:** Add `--api` flag for JSON-only scaffold generation (#1153).
- Add transactional test isolation for database tests (#1055).

### Fixed

- **ui:** Add semantic CSS classes to all framework widgets + fix wizard stepper connector([fae4746](https://github.com/madmax983/autumn/commit/fae474607207a4ec1d90771a87da0f2ad9ed67f0))
- Skip E0119 time 0.3.48 coherence regression in semver check([0abf525](https://github.com/madmax983/autumn/commit/0abf525f3e0112c903942c1b2d3435457d30b08b))
- Update chromiumoxide 0.7→0.9 to drop removed byteorder dep([dcc7826](https://github.com/madmax983/autumn/commit/dcc782689deca46665d56ba0961db3431c8cfd11))
- Pin time <0.3.48 to avoid E0119 coherence regression([dba2a30](https://github.com/madmax983/autumn/commit/dba2a30fe5cb02df74fee2d07738769486d6f7af))
- Hoist outer out.push('\n') after if/else chain to fully satisfy branches_sharing_code([7b1045e](https://github.com/madmax983/autumn/commit/7b1045ec5975a07c656f01258f6fefbeff95dadf))
- Hoist shared out.push('\n') after if-else to satisfy clippy::branches_sharing_code([60bab65](https://github.com/madmax983/autumn/commit/60bab6548c2f76b4b6a9072d905528f38ffca7e4))
- SEO collision guard covers scoped groups; TOML comma placed before inline comment([41affeb](https://github.com/madmax983/autumn/commit/41affebd1e1862c2283a94abcff287a06c9f225e))
- Skip autumn-storage-s3 semver check on aws-runtime E0282 upstream regression([2d76f05](https://github.com/madmax983/autumn/commit/2d76f05c015aace413f542609891b7fbae4c9904))
- Widen aws-runtime exclusion to all of <1.7 (1.7.3 same E0282 bug)([013ff76](https://github.com/madmax983/autumn/commit/013ff76062a403e272cbfbb6908ec556c7bd30d3))
- Coalesce local pending-window retry when duplicate owns key; pin aws-runtime([d18cb55](https://github.com/madmax983/autumn/commit/d18cb553d19132b15134c7df8ca4758047e4d4d5))
- Multiline TOML comma and scoped path collision normalization([a91c37c](https://github.com/madmax983/autumn/commit/a91c37c5f9c1122a1fd75be7c5a8557209a4e433))
- **tests:** Update seo test to match truncation-not-sitemapindex behavior([4b9e3ec](https://github.com/madmax983/autumn/commit/4b9e3ec6aaee504c723171a9c6dffb5f4a1fe87f))
- Eliminate stale-recovery race window for pending-window unique keys([ee08e56](https://github.com/madmax983/autumn/commit/ee08e56001c052d003b70f819355d08cd77cedcc))
- Inbound-mail build and Redis pending-window retry dedup([29d5296](https://github.com/madmax983/autumn/commit/29d5296e9ec997c96ffa3a80eddc39356ae37fc2))
- TTL-unique dedup and retry unique_key regression([204a578](https://github.com/madmax983/autumn/commit/204a578641041f908ad5deda8aa62a22b012c4e6))
- Security hardening and atomic dedup for retry([645cd63](https://github.com/madmax983/autumn/commit/645cd63ce1268418df0ccc82ab82f4f26541536b))
- **cli:** Generate schema for oauth_identities and fix oauth test syntax([f8a227e](https://github.com/madmax983/autumn/commit/f8a227e4819ac3abae6d802aa6f2737361451add))
- Replace useless format! with concat!.to_owned() in render_oauth_docs_file([b2693c4](https://github.com/madmax983/autumn/commit/b2693c48c8847d884797fc566d3545c18f5cc53f))
- Address Codex P2 review comments on OAuth2 configuration([363c37f](https://github.com/madmax983/autumn/commit/363c37f84e5c1474f1da98750298eba09e8c7198))
- Silence --all-targets clippy warnings in test code([29ad0e0](https://github.com/madmax983/autumn/commit/29ad0e08dd822001e7654c956320622f02b411f6))
- Use batch_execute for multi-statement migration in feature_flags_pg_integration test (#1041)([ca23e85](https://github.com/madmax983/autumn/commit/ca23e851c6488a47bd4e6343739bcd9020fcac15))
- Keep release gate from mutating changelog (#763)([516c663](https://github.com/madmax983/autumn/commit/516c663c0f804c79f00377bc84639bd3aa7864e2))

### Documentation

- Agent plugin (#1164)([cda6e78](https://github.com/madmax983/autumn/commit/cda6e78fccc8387169fb040d12c56dd485e4c31c))

### Styling

- Rustfmt — wrap long tracing macro string literals([5f35362](https://github.com/madmax983/autumn/commit/5f353624ffe4f6059f3ade1954148ecaeffa7b37))
- Apply cargo fmt to all workspace files([e77ce4b](https://github.com/madmax983/autumn/commit/e77ce4ba61d2c1c2e802d0f9f3d6beca53b3ba1d))

### Miscellaneous

- **deps:** Bump actions/upload-artifact from 4 to 7 (#1067)([b88a095](https://github.com/madmax983/autumn/commit/b88a095e8e32a1ec6c3d5bde0c30dc762acde57c))
- **deps:** Update pulldown-cmark requirement from 0.12 to 0.13 (#1068)([d60694d](https://github.com/madmax983/autumn/commit/d60694d5f6de66a59f5353c95c812ed464ab90a5))
- Clippy([1d2ab8c](https://github.com/madmax983/autumn/commit/1d2ab8c6fa8ddf8970e51172ad9787596c7029db))
- Clippy([38417bb](https://github.com/madmax983/autumn/commit/38417bb7cc8928374568803fb7ab455311fe72ab))
- **deps:** Bump django (#760)([ef1af3a](https://github.com/madmax983/autumn/commit/ef1af3a1cc48d348d1213a03d0fe1c0a0595e465))
- **deps:** Bump actions/download-artifact from 4 to 8 (#745)([afee5bf](https://github.com/madmax983/autumn/commit/afee5bfa616c4ef24ba485bb85d5453ffd14e0e4))
- **deps:** Bump actions/upload-artifact from 4 to 7 (#744)([b6b028c](https://github.com/madmax983/autumn/commit/b6b028cf71a7efee75d1437d2edc1b91f7b5313a))
- Changelog and release notes([367bcd3](https://github.com/madmax983/autumn/commit/367bcd365df380f974f9cb6d943467e8d9c672a6))
## [0.4.0] - 2026-05-12

### Added

- **webhook:** Add signed webhook intake with durable replay protection (#737)([7bcd8d4](https://github.com/madmax983/autumn/commit/7bcd8d4bec289e94bbc5b66ed32c29697661a0d6))
- Standardize JSON errors as problem details (#722)([42c6501](https://github.com/madmax983/autumn/commit/42c6501675e8b052ecfd0aa873344674836f2f0c))
- **release:** Gate crates.io releases with compatibility checks (#594) (#715)([6134619](https://github.com/madmax983/autumn/commit/613461928e000b65de4b89404b56eac14e3996bf))
- Make router-constructing functions generic over state (#712)([76d9c85](https://github.com/madmax983/autumn/commit/76d9c85a05ace4f75b6999d2932aa6d2e9f3e390))
- **cli:** Add `autumn generate admin` for autumn-admin-plugin adapters (#709)([991fb4a](https://github.com/madmax983/autumn/commit/991fb4a70985589fb4a8a1f4222389c33cccc6d2))
- **admin:** Add jobs dashboard for background work (#688)([e473d5f](https://github.com/madmax983/autumn/commit/e473d5ff518bcea6d8c504d4b6db75c0f3682099))
- **plugins:** Add plugin conformance checks — autumn plugin-check CLI and library API (#692)([287c8fa](https://github.com/madmax983/autumn/commit/287c8fab3acfd18e4c95858131323f9dd462e415))
- **a11y:** Add accessible form helpers, /actuator/a11y endpoint, and accessible scaffold (#678)([18b8a6d](https://github.com/madmax983/autumn/commit/18b8a6dd147a233388f95483b887c4f7617ef427))
- **cli:** Add scaffold metadata flags and regenerate bookmarks (#670)([d085f9b](https://github.com/madmax983/autumn/commit/d085f9bb81bcd67e296347ebc933b4db1b4736d6))
- **scheduler:** Coordinate scheduled tasks across replicas (#644)([2bb5015](https://github.com/madmax983/autumn/commit/2bb5015b3efa0bc51ede10acc5894a0aef3381fe))
- Broadcast (#636)([9212b52](https://github.com/madmax983/autumn/commit/9212b520621f3f216000a8b6d52643fe26431738))
- Add ChannelAuditSink to broadcast audit events over websockets (#507)([4ad4f86](https://github.com/madmax983/autumn/commit/4ad4f86f19bef14ce02c81b151d99abd3039dd0f))

### Fixed

- **db:** Enforce replica_fallback in readiness and read routing (#732)([82cfda3](https://github.com/madmax983/autumn/commit/82cfda3a772dcec4751cb5f39a00c21a76b5418a))
- **csrf:** Remove misplaced CsrfToken doc block above CsrfFormField (#672)([4283c9b](https://github.com/madmax983/autumn/commit/4283c9b1e8d8d10982ef1b5353462282b165fa16))
- **doctor:** Avoid executing project-local Tailwind binary (#615)([519f9d9](https://github.com/madmax983/autumn/commit/519f9d9bd65eef5475268ce2587fd2c5de5c716b))
- **auth:** Pass create payload into policy checks (#614)([f24fa55](https://github.com/madmax983/autumn/commit/f24fa553962623272407dad18ba2413a751e866c))
- **cli:** Make scaffold generation auth-safe by default (#613)([f9e629d](https://github.com/madmax983/autumn/commit/f9e629d40608e70f51d7124376973afb657d82ef))
- Resolve broken intra-doc links in lib.rs and tokens.rs (#589)([11e9e52](https://github.com/madmax983/autumn/commit/11e9e52c6e7070cf17b6ccc42b5454fd149e3b15))

### Changed

- Flatten error page filters and reuse home link (#684)([b14d9c4](https://github.com/madmax983/autumn/commit/b14d9c4d27f72a0a4e4d06257a24ccda748be267))
- Remove AppState circular dependencies from tests (#570)([8864634](https://github.com/madmax983/autumn/commit/88646344943f4b2c7aa6cf94a4708b66afa5ee87))
- **actuator:** Replace deeply nested if-let blocks with let-else guard clauses (#549)([5b3c73e](https://github.com/madmax983/autumn/commit/5b3c73e7ff3fabcf61ea74322c9f6ba340aa2740))

### Documentation

- Certify first-run docs against published crates (#720)([3adf8be](https://github.com/madmax983/autumn/commit/3adf8bec65c2339fca4ab29487add5d2f4acc86a))
- **todo-app:** Mention scaffold generator alternative (#668)([340f50b](https://github.com/madmax983/autumn/commit/340f50b9f7b628830d99eb830d4ef0582f4e3f7d))
- Fix broken intra-doc links across workspace (#551)([3eb08b1](https://github.com/madmax983/autumn/commit/3eb08b165f05806d46a2a71804613d8d76168aa7))
- Update CHANGELOG.md for v0.3.0([2a0c7f3](https://github.com/madmax983/autumn/commit/2a0c7f3deb09aba19bfe6cf16dff822810e9eac1))

### Testing

- **cli:** Add live scaffold HTTP verification (#665)([718f6c7](https://github.com/madmax983/autumn/commit/718f6c7916d3c32b28346dff09f1d9c8356cc14a))
- **auth:** RED - add failing tests for API token authentication (#627)([ff38f9c](https://github.com/madmax983/autumn/commit/ff38f9cadd50ae39b4b880d1549c946e3bb2dd70))
- **i18n:** RED phase — failing tests for Fluent-based i18n module (#503) (#567)([f53eb1f](https://github.com/madmax983/autumn/commit/f53eb1f1cecba28d45e4869313422c0d67bfea5b))

### Miscellaneous

- **deps:** Update getrandom requirement from 0.3 to 0.4 (#634)([9dcb20a](https://github.com/madmax983/autumn/commit/9dcb20a86827672ecf169bfa4d586a7e37fb7f8e))
- **deps:** Update lru requirement from 0.17.0 to 0.18.0 (#526)([1cc652c](https://github.com/madmax983/autumn/commit/1cc652c48e20e7acd82f4b0c78ac163e7c261863))

### Warden

- Fix TOCTOU vulnerability in file storage (#547)([bce10da](https://github.com/madmax983/autumn/commit/bce10da3c5e73ffc95d5ae9b9049bbe06a59e8e4))

### Autumn-cli/src/templates/release/Dockerfile.tmpl

- 2 now builds from rust:{{rust_version}}-bookworm and installs cargo-chef, so rendered release images use the declared 1.88.0 MSRV instead of Rust 1.86.([f073899](https://github.com/madmax983/autumn/commit/f0738991cb83dada0b05406f35c517c7f96fdf46))
## [0.3.0] - 2026-04-27

### Added

- Add autumn-admin-plugin with auto-generated CRUD UI (#455)([4486405](https://github.com/madmax983/autumn/commit/44864052036aba83740b664595cbbef1f93bdfa2))
- **audit:** Add first-class structured audit logging API (#437)([4ac0f7c](https://github.com/madmax983/autumn/commit/4ac0f7c92b3271616175f80216c8fa4b535dca13))
- Add hx_location support to HxResponseExt (#408)([0f6ea9d](https://github.com/madmax983/autumn/commit/0f6ea9db578b6ed5ac54072b86dca458d90fa4f6))
- **security:** Htmx-friendly default CSP for secure-headers (S-049)([a71f1af](https://github.com/madmax983/autumn/commit/a71f1af905ea6f7aaf53c3201912960775b2d94e))
- **security:** Built-in per-IP rate limiting (S-047)([68ccada](https://github.com/madmax983/autumn/commit/68ccadab4d68757d9c924e8250fd96b783ac9159))
- **security:** CSRF error body + route-specific exempt_paths (S-046)([8ecc78e](https://github.com/madmax983/autumn/commit/8ecc78ea9cf7ba27a22ac75e6a2f81e52a83bd64))
- **app:** Complete raw axum route mounting coverage and docs([55ae63a](https://github.com/madmax983/autumn/commit/55ae63a0e07b8a5540970e818564716a8cbf0f9e))
- **app:** Add AppBuilder::layer for custom Tower middleware (S-049)([62c33a2](https://github.com/madmax983/autumn/commit/62c33a2ef0a601c0129babda2943ba21e63c89f9))
- Trait-based subsystem replacement for config / DB / telemetry / session (S-053)([89683ed](https://github.com/madmax983/autumn/commit/89683edbf261f7ce580efd00fea4389b5c4556e3))

### Fixed

- Patched MSRV([f56a82d](https://github.com/madmax983/autumn/commit/f56a82de71d57c9bee09db6a1862140535d67cfb))
- Crate version issue([54fcd7b](https://github.com/madmax983/autumn/commit/54fcd7bfa31e9a1c54c7a9eafdbfeac3da8c7c1a))
- Vendor swagger-ui([6cbac95](https://github.com/madmax983/autumn/commit/6cbac95187ded4c20abea257582163ccd02de8b1))
- Multipart_rejection_to_error([516dbc5](https://github.com/madmax983/autumn/commit/516dbc5eedf2188ce8e5bf32dea41f9a83f1875e))
- Resolve intra-doc link warnings in cargo doc (#450)([38b743d](https://github.com/madmax983/autumn/commit/38b743d715b17e3e4463a2ba47e5319a8ecb4b1c))
- **dev:** Serve live-reload script from /__autumn/live-reload.js([1e82da0](https://github.com/madmax983/autumn/commit/1e82da0e7d2438841764b8b01938eabbc4283bda))
- Resolve clippy linting errors in error_page_filter.rs([b3ca678](https://github.com/madmax983/autumn/commit/b3ca678d1d62d4812200ee32e90c6cee8a864175))
- **rate-limit:** Bypass when no identifiable client (P1)([475a6e6](https://github.com/madmax983/autumn/commit/475a6e6c05f04fc9ab9041c45946779026ca848b))
- **rate-limit:** Untrust forwarding headers by default; fix sweep([784e9a7](https://github.com/madmax983/autumn/commit/784e9a705a80af060079a926b4991be644cb5678))
- **cors:** Reject wildcard+credentials, warn on malformed values (S-048)([0477f49](https://github.com/madmax983/autumn/commit/0477f49092573721bf70895600f7a12fdca9edbf))
- **S-049:** Review polish + apply custom layers in static build mode([fa0f1d0](https://github.com/madmax983/autumn/commit/fa0f1d05193f66962285db3e444b4a226ddc87b1))
- Eliminate panic risks in config merge and test telemetry fallback([50ad773](https://github.com/madmax983/autumn/commit/50ad7730f90a39d1f4e6f5d85818b0a976192713))
- Restore fail-fast session validation for the default session path([069c509](https://github.com/madmax983/autumn/commit/069c5099f3ed7688bba50cb902a418fac9293c51))
- Bypass session config validation when custom store is configured([d3b4fad](https://github.com/madmax983/autumn/commit/d3b4fade99f4e2a4048a2040f1da151d45871980))
- Address Codex review on PR #382 (P1 + P2)([7991aa3](https://github.com/madmax983/autumn/commit/7991aa3c179c02d8b9425c684e600216c5c65465))
- Expose telemetry module + TelemetryGuard::disabled() publicly([81711fc](https://github.com/madmax983/autumn/commit/81711fc1deb6d059aaf8c4e937d57ef3cab4a113))

### Performance

- **config:** Optimize levenshtein to use a single vector (#419)([896611f](https://github.com/madmax983/autumn/commit/896611fd5db14214b124452fe6739c3026b20a58))
- **rate_limit:** Use zero-cost numeric HeaderValue conversion (#405)([2edf7fe](https://github.com/madmax983/autumn/commit/2edf7fe4f0e85361d1b7f1c379bf164b6eebb6bf))

### Changed

- Implement Display for Schedule and simplify formatting (#418)([d90bb68](https://github.com/madmax983/autumn/commit/d90bb68653415d0a0194730a428cc6dbf8790023))
- **app:** Sealed IntoAppLayer trait for readable compile errors([ea5dd83](https://github.com/madmax983/autumn/commit/ea5dd833ecbbc285b9f76de9b3c5c5b810f760ab))
- **config:** Extract parse_env_option_string helper([13f97f9](https://github.com/madmax983/autumn/commit/13f97f9e2d5ca8c82c008b70c6711b442bbe2648))
- **config:** Extract parse_env_option_string helper([36650dd](https://github.com/madmax983/autumn/commit/36650dd279e9badd2d97da311dd9d6e6dc0a9b70))

### Documentation

- Skill([30b5e21](https://github.com/madmax983/autumn/commit/30b5e21798f5d9403f5fea2d95f5d126e24ebf0c))
- Fix broken rustdoc intra-doc links (#475)([99e1e2d](https://github.com/madmax983/autumn/commit/99e1e2d6e5fbb8fbeddadc623597b8265ca99c54))
- Add SemVer stability policy and MSRV-alignment CI check (#433)([54692c9](https://github.com/madmax983/autumn/commit/54692c9fc8f4deb669b8c5871fa02017db8b0201))
- Add Vantage spec for configurable dev watcher (#422)([0abb47e](https://github.com/madmax983/autumn/commit/0abb47ee4abf9a9fe05baaaa556dbce23d1d74f4))
- Append DX audit report for primitive return type compilation errors (#421)([fa30f20](https://github.com/madmax983/autumn/commit/fa30f20629ad8bb1a9804a4d9bb651e275e9250a))
- Verify tests for __check_secured (#417)([c71ccee](https://github.com/madmax983/autumn/commit/c71ccee82ef5b77542bbe8ed530ec82680db86d1))
- Add Vantage spec for middleware introspection (#409)([1b3d260](https://github.com/madmax983/autumn/commit/1b3d26067dea7cd96322d196eff5c81115d46b78))
- Drop stale status block from README (#379)([eef956e](https://github.com/madmax983/autumn/commit/eef956ea11f6715fd83e8c721d62962ab1e226b8))
- Update CHANGELOG.md for v0.2.0([7b4d922](https://github.com/madmax983/autumn/commit/7b4d922aa2abf01d8aa55a483032434b2f70b6ed))

### Styling

- Rustfmt merge resolution([9831b22](https://github.com/madmax983/autumn/commit/9831b2228c4b9181be6cf6ba4780f5c4c72e928b))
- Rustfmt([86e9b4c](https://github.com/madmax983/autumn/commit/86e9b4c5e9d8034317267cdd46a1b9da71cb2e83))
- **security:** Rustfmt fix for CSRF error response([b30d69d](https://github.com/madmax983/autumn/commit/b30d69d232392c3cb842d65664f7fefab29bceab))
- Apply rustfmt to preflight test([d61fff4](https://github.com/madmax983/autumn/commit/d61fff459d2a12bee438c54f29c5f0612463c453))

### Testing

- Add test coverage for HEAD requests in fallback_404_handler (#485)([d5a2da8](https://github.com/madmax983/autumn/commit/d5a2da8722cfe203695b8fa2227724fc3a2beac1))
- Add test coverage for pagination mutants (#469)([637fb83](https://github.com/madmax983/autumn/commit/637fb8318641401938b9a0e82f34ddbe6790955b))
- **flash:** Strengthen flash module tests to kill surviving mutants (#430)([2fb18c5](https://github.com/madmax983/autumn/commit/2fb18c54b00e0b235317075cad7d6db55a64f525))
- Add test coverage for hash_password (#416)([94adba4](https://github.com/madmax983/autumn/commit/94adba4c824e7897e43d294d5f902a5934b817bc))
- Acknowledge existing coverage for fallback_404_handler (#415)([7bfea47](https://github.com/madmax983/autumn/commit/7bfea4794051987ffb16535aaa15fe31b2f89615))
- Add coverage for init_with_telemetry (#413)([378f17c](https://github.com/madmax983/autumn/commit/378f17cd4aebf3d0247552c1e588dd0df3d1f417))
- Add test for live_reload_state_handler (#411)([7f3e64c](https://github.com/madmax983/autumn/commit/7f3e64cfabfa2d912dfe4c7fc78fd8ecfc9f968f))
- Close mutant gap in DieselDeadpoolPoolProvider::create_pool (#406)([759c5ee](https://github.com/madmax983/autumn/commit/759c5eeb4b928f5cb6cb6dd4161d4e85bef8b772))

### Miscellaneous

- Version tags([3d5c171](https://github.com/madmax983/autumn/commit/3d5c171e5f5d738cb89af87b12112a9ce62637f5))
- Version tagging([8c62662](https://github.com/madmax983/autumn/commit/8c626629e6939f95dc242e592cdc8ff17c23ebb7))
- PR feedback([86ebfd8](https://github.com/madmax983/autumn/commit/86ebfd8db6153f76592fd927e2b6c3354808d379))
- Cleanup([169c894](https://github.com/madmax983/autumn/commit/169c894b37e430fdbe06bc30dbb157288f0d01cf))
- Trigger on trunk-dev push and pull_request (#376)([8a46d2c](https://github.com/madmax983/autumn/commit/8a46d2c0aa748513c1f7d01a25774c1b3c6a500b))
- Fmt([660cf10](https://github.com/madmax983/autumn/commit/660cf10f3c78b0187b1aa02613a75c8e1dd1cb51))
- Use RwLock instead of Mutex for AppState extensions (#370)([f47e46d](https://github.com/madmax983/autumn/commit/f47e46d2a068f3daac9e8a615df2c2a0c178b263))

### Refactor

- Re-export axum::extract::State to hide axum dependency([d35ccc5](https://github.com/madmax983/autumn/commit/d35ccc50c32f44f811b18a9427d88c9160c0cc5c))
- Re-export axum::extract::State to hide axum dependency([407c4ca](https://github.com/madmax983/autumn/commit/407c4cae415cbe2b19b2d6c8ead0723ccbaab442))

### Merge

- Resolve conflicts with trunk-dev (rate-limit + CSP features)([5b0397d](https://github.com/madmax983/autumn/commit/5b0397d99267822cacc3e27f973135e554c35897))

### Sentry

- Eliminate unchecked unwraps (#445)([79c7caf](https://github.com/madmax983/autumn/commit/79c7caf774294edbdb246e4058afd2dbf9fda21b))
## [0.2.0] - 2026-04-19

### Added

- Bridge Channels pubsub with SSE streams for htmx (#344)([8497afd](https://github.com/madmax983/autumn/commit/8497afda4257077ef0a3ce41df025646f02b3c89))
- Add HxResponseExt trait for fluid HTMX response header configuration (#274)([fbe8630](https://github.com/madmax983/autumn/commit/fbe8630abff0f4da30ff85abac4651eb610be8f5))
- Add harvest topology escape hatches (#223)([e55a1be](https://github.com/madmax983/autumn/commit/e55a1be80dd9186fe175f488aff5188842c154b0))
- **actuator:** Add prometheus metrics exporter (#164)([351d3da](https://github.com/madmax983/autumn/commit/351d3daed0830e1fb465c747a64899c0b6d81f5a))
- **error:** Add 500 error constructors to AutumnError (#157)([02396e9](https://github.com/madmax983/autumn/commit/02396e9e9bb5f2210590c28d3cb2fc53f82c9182))
- **harvest:** Implement Phase 5 signal delivery and query registry (#113)([c4ab5b8](https://github.com/madmax983/autumn/commit/c4ab5b8db2b0a25cb41488c129c57c5495a82ff8))
- **harvest:** Add replay-aware child workflow command support (#98)([58c0bb3](https://github.com/madmax983/autumn/commit/58c0bb311b90bef8f2808a90f812319342f6a616))
- Add autumn-harvest durable workflow engine (#57)([aa10042](https://github.com/madmax983/autumn/commit/aa10042cb95cdda57b175394fa211460e340a688))
- Implement autumn-harvest Phase 1 — durable workflow engine foundation (#43)([819e993](https://github.com/madmax983/autumn/commit/819e9931e32e9982d5615134613dd080cf3c9564))
- Add v0.2 features — actuator endpoints, migrations, error pages, hybrid rendering Phase 2, raw Axum escape hatch (#37)([df31508](https://github.com/madmax983/autumn/commit/df315085c4adc4fb0720389e817e9a7ad6cd34f3))
- **macros:** Add #[service] macro for cross-model orchestration (#36)([114f292](https://github.com/madmax983/autumn/commit/114f29246f031fab85770593ec7101415d491758))
- **wiki:** Add REST API via api macro([fefbcf6](https://github.com/madmax983/autumn/commit/fefbcf6304044f5223ed31db6fc695601edfa34a))
- **macros:** Generate CRUD API handlers from api = "/path"([a13971b](https://github.com/madmax983/autumn/commit/a13971bfe21aed8304859fbb61194fec49d2d21b))
- **macros:** Parse api = "/path" in #[repository] attribute([8e701e9](https://github.com/madmax983/autumn/commit/8e701e972d9499f50955051a1838dac32c60f47e))
- Hooks integration, wiki example, and i64 migration (#29)([017f2ce](https://github.com/madmax983/autumn/commit/017f2cef78d7989633cbae193e21627c8c7c2b12))
- **hooks:** Add UpdateDraft<T> and DraftField<'a, T> types (#28)([0b853f2](https://github.com/madmax983/autumn/commit/0b853f222cede82fb721fd50a0a82182682d6108))
- Hybrid rendering Phase 1 — #[static_get] macro and StaticFileLayer (#25)([f2b62dc](https://github.com/madmax983/autumn/commit/f2b62dc9ca19c4fc374f9a42ec8c7f9a2b64dd50))
- Add bookmarks example showcasing v0.2 features([3fe79f0](https://github.com/madmax983/autumn/commit/3fe79f0719efb26144913c4b6beeaf9afb443d14))
- Add blog engine example([f52eb1f](https://github.com/madmax983/autumn/commit/f52eb1f468517a796c63196eb79e6b552ad4bf07))

### Fixed

- **session:** Prevent cookie tossing vulnerability in session cookie extraction (#286)([5c854ca](https://github.com/madmax983/autumn/commit/5c854ca1e47894da2e5566fc4ab0a8e6207135e3))
- Handle integer overflow gracefully in parse_duration (#236)([c99ad94](https://github.com/madmax983/autumn/commit/c99ad94cca2ed3da930eaeae9ee11a834d7f77c9))
- **cli:** Handle missing tailwind cli gracefully in build.rs template (#226)([fc85378](https://github.com/madmax983/autumn/commit/fc85378cb81e5123f56a233a40109ee9a27ecb76))
- Harden harvest listen notify sql (#174)([8ff0359](https://github.com/madmax983/autumn/commit/8ff0359294b61a38f89a631b16a322d0747a1ee1))
- Re-export Path extractor in prelude for better DX (#124)([076f574](https://github.com/madmax983/autumn/commit/076f5749f9c55e18f5e77f3db56ccab7ae324745))
- **wiki:** Use PageForm for create route to avoid missing slug field([e644b28](https://github.com/madmax983/autumn/commit/e644b28d06581cad9d874c4489e422a5e14aa580))
- Bookmarks example CSS, form submission, and missing files (#24)([6528ca7](https://github.com/madmax983/autumn/commit/6528ca7fb9b49c400e70953398b9dc2a64313885))
- Resolve #[repository] macro path issues for downstream crates (#23)([616855b](https://github.com/madmax983/autumn/commit/616855b1f0c302dc39766a01fe93e78a8ea16440))
- Update trybuild expected error for #[model] on enum([347e868](https://github.com/madmax983/autumn/commit/347e86879f6b1155f522701554fed7a550200c9b))
- Resolve CI lint errors (needless raw string hash, unused import)([401b12b](https://github.com/madmax983/autumn/commit/401b12bdc60691e8b4f6d64228ade3cfd4ffe0fc))
- Add version requirement to autumn-macros dep for crates.io publish([6216345](https://github.com/madmax983/autumn/commit/6216345e0ad9de6f1c2ea0db477dab1744672b69))

### Performance

- Optimize levenshtein to avoid intermediate string allocations (#131)([6dfc1f4](https://github.com/madmax983/autumn/commit/6dfc1f4ee8080e8bff501efeab2da1d4d07a9caf))
- **metrics:** Optimize compute_percentiles to O(N) using select_nth_unstable (#95)([470a0b4](https://github.com/madmax983/autumn/commit/470a0b41fb5317b204e3f491fe4cf8c47e19dbce))

### Changed

- **router:** Extract RouterContext and flatten try_build_router_inner (#235)([a55c06b](https://github.com/madmax983/autumn/commit/a55c06be5f84c72f636fbe7413172f04b78b7571))
- **middleware:** Replace `is_some()` + `unwrap()` with `if let` in `exception_filter.rs` (#71)([17b4676](https://github.com/madmax983/autumn/commit/17b46760757b7fcd7ce650ccae1c2a70dbcc3146))
- **bookmarks:** Replace hand-written API routes with api macro([c66c2e3](https://github.com/madmax983/autumn/commit/c66c2e3f2bef5dd19f51b76d1aef8dcaecf97c4c))

### Documentation

- Add known bug note to Channels panics (#363)([c07d4db](https://github.com/madmax983/autumn/commit/c07d4db5ae16d12f7428860af3c05179abc640a4))
- Clean up bug references in channel docs and tests (#311)([8690e9d](https://github.com/madmax983/autumn/commit/8690e9d7a2447e33b1c7c1df47d32ba94b4d2394))
- Add spec for audit logging (#277)([51da75f](https://github.com/madmax983/autumn/commit/51da75fbf6720fec5571b2e04cbe6a7e1c28a4f3))
- Add DX Audit Report (#251)([25abfdd](https://github.com/madmax983/autumn/commit/25abfdd3659b8c9329b18e25d2b903488b169223))
- Add vantage spec for websocket support (#219)([49edbda](https://github.com/madmax983/autumn/commit/49edbda4ac209a18ba4c5e5c88a6c5b7de03b020))
- Add spec for migration management (#183)([809ac97](https://github.com/madmax983/autumn/commit/809ac97bf1b1a08a81f5bb4a27bc055b63d1ebab))
- Clean up AppState field noise and add module-level docs (#145)([8ff7424](https://github.com/madmax983/autumn/commit/8ff7424807367dcd08d76c80a149288473599220))
- Add vantage spec for custom middleware (S-049) (#156)([f3086dd](https://github.com/madmax983/autumn/commit/f3086dd12f69994ff1d5da0db40202449c1c38c5))
- Add wasm roadmap design (#60)([6c01f76](https://github.com/madmax983/autumn/commit/6c01f76a46069a9044313c432ecd866486d89816))
- Refresh trunk docs and example guides (#41)([48d4b7e](https://github.com/madmax983/autumn/commit/48d4b7e9e66c3b4e53479bd007d5076d723a74e5))
- Add autumn-harvest Phase 1 implementation plan([d091fed](https://github.com/madmax983/autumn/commit/d091fed8fd1b560751abb59333db3db8fa4aed8e))
- Add CRUD API macro implementation plan([1934e44](https://github.com/madmax983/autumn/commit/1934e44aad842e816210dbf9bed76b3418d9b0ff))
- Add CRUD API macro design plan([98c55f8](https://github.com/madmax983/autumn/commit/98c55f885a2f73e99d18f7fd51e18b1ae11e7a80))
- Update CHANGELOG.md for v0.1.0([0ff87b5](https://github.com/madmax983/autumn/commit/0ff87b5fae52bd4b9a710e7c596bbc2227afb31d))

### Styling

- Cargo fmt([f1fe44d](https://github.com/madmax983/autumn/commit/f1fe44d739406f42813b0d954e6a04e25f331aec))

### Testing

- **dag:** Increase DAG builder coverage (#353)([84487ce](https://github.com/madmax983/autumn/commit/84487ce6872078bf517cd92b0232c67468bbeb54))
- Add fallback_404_handler tests for root path and query params (#348)([75c6d76](https://github.com/madmax983/autumn/commit/75c6d7653bdfba13968072d5b069e5f3cd29b642))
- **htmx:** Add edge case tests for HxResponseExt and verify_password (#312)([aacbb30](https://github.com/madmax983/autumn/commit/aacbb305e2bfe589855ba753750b6bede133c8c6))
- Update auth_dos assertion to prove fast response (#303)([46a8fd5](https://github.com/madmax983/autumn/commit/46a8fd5cee00e0eb09c5766142c9179564bfe05b))
- **security:** Add CTF-themed security regression suite (#278)([d07e8bd](https://github.com/madmax983/autumn/commit/d07e8bdf3dbc10fd58d6bb72ff4fc8ce7416a4e6))
- Verify csrf timing fix is verified in existing test (#262)([cbc9bf1](https://github.com/madmax983/autumn/commit/cbc9bf1dfd8076964b35e713f068a1d3fb72137d))
- **security:** Add test for referrer_policy configuration (#213)([f5e8cf7](https://github.com/madmax983/autumn/commit/f5e8cf7548d1b631519796591f984187a7cc366d))
- Add unit tests for Patch<T> enum state matchers (#210)([ee12301](https://github.com/madmax983/autumn/commit/ee123011933d905aa4f340e8adf798d547166395))
- **middleware:** Test state file reading in live reload handler (#143)([1ba174e](https://github.com/madmax983/autumn/commit/1ba174e178b776cb29c9ca5e5a70fec9ee35d699))
- Add missing tests for AutumnError methods in autumn-web (#109)([a821a19](https://github.com/madmax983/autumn/commit/a821a196b0a0e7fd203649ec51d376a6dadd2e61))
- Add compile-pass for repository with hooks + api combined([14847aa](https://github.com/madmax983/autumn/commit/14847aa00ed18d88df55409dbe59f33004dd7578))
- Kill 8 mutation testing survivors in config module (#26)([7a14dc3](https://github.com/madmax983/autumn/commit/7a14dc3f170c8a2657bf03fae2296a6f870f1c08))

### Miscellaneous

- Extract autumn-harvest to separate repo([ba4e342](https://github.com/madmax983/autumn/commit/ba4e3421d87eced7ff8629ffa0b572adb4c28341))
- Temporarily remove reddit-clone example pending autumn-harvest publish([e765eac](https://github.com/madmax983/autumn/commit/e765eac199807e7546de185e3ddc7690f169c56d))
- Clippy clean-up (#338)([89d0d1b](https://github.com/madmax983/autumn/commit/89d0d1be421d71e7d0c211fc04d01077993bbdc3))
- Python cleanup([3186068](https://github.com/madmax983/autumn/commit/3186068c8f95cf6a91b8d8939cfdc6722a9fcbdd))
- Cleanup([3379bcd](https://github.com/madmax983/autumn/commit/3379bcde055bfc513ec72a45823e2e44b8f28c36))
- Clean up files([0873ccb](https://github.com/madmax983/autumn/commit/0873ccba410543a40c2c8f83926e5088011e80df))
- **deps:** Update testcontainers requirement from 0.23 to 0.27 (#270)([072f4c9](https://github.com/madmax983/autumn/commit/072f4c9c9dd02ad880f3a4c85123fd1896bd3b9a))
- **deps:** Bump softprops/action-gh-release from 2 to 3 (#269)([67f56a4](https://github.com/madmax983/autumn/commit/67f56a43b3572221639ff428b635c6c3519307ca))
- **deps:** Update crossterm requirement from 0.28 to 0.29 (#79)([529c195](https://github.com/madmax983/autumn/commit/529c1950f55b4c92bf7cebfba31b28669c1a197d))
- **deps:** Update bcrypt requirement from 0.17 to 0.19 (#75)([edb7248](https://github.com/madmax983/autumn/commit/edb72480fa322d2f7f8618febb055f95748575a2))
- **deps:** Update tokio-cron-scheduler requirement from 0.13 to 0.15 (#78)([a4ee049](https://github.com/madmax983/autumn/commit/a4ee049cc513550183b572b4d74a903835dfbc5c))
- **deps:** Update toml requirement from 0.8 to 1.1 (#14)([80eb617](https://github.com/madmax983/autumn/commit/80eb617cef4ff93e6ae9a7e861b10932cd4afb6f))
- **deps:** Update sha2 requirement from 0.10 to 0.11 (#17)([514578a](https://github.com/madmax983/autumn/commit/514578ac04c8d9c4c461f26b304fbd6ca322b460))
- **deps:** Update reqwest requirement from 0.12 to 0.13 (#15)([80dc749](https://github.com/madmax983/autumn/commit/80dc749048f198ec8a1c0101bdb3254f37161185))
- **deps:** Bump codecov/codecov-action from 5 to 6 (#12)([a5b4bd0](https://github.com/madmax983/autumn/commit/a5b4bd0f9ea7a8712d33b61826add456128ba8f9))
- Clean up test files and encoding issues([63cc397](https://github.com/madmax983/autumn/commit/63cc39743d6eb60f8dc07197a19463f36304eedb))
- Fmt([15ac48d](https://github.com/madmax983/autumn/commit/15ac48d6ddfb5c91c00ec087d192060afe666668))

### Docs

- Fix intra-doc links and add error examples (#88)([0e9dbad](https://github.com/madmax983/autumn/commit/0e9dbadd9fbe988ea2f42a29a25650dfa4fa22a3))

### Echo

- Fix DX audit findings (Macros, 404 Body, Tailwind Warnings) (#294)([7a47630](https://github.com/madmax983/autumn/commit/7a47630986536d36eae87e0cc2a6fed0d233eca6))
- DX Audit for README Setup (#241)([9938abd](https://github.com/madmax983/autumn/commit/9938abdf837aea1b5288d634c0be43d473ccacc1))
- DX Audit Complaint & Fix (#195)([1b80080](https://github.com/madmax983/autumn/commit/1b80080775c63cb88b8a5b91d26e9dd0bfa229a7))
- DX Audit Complaint & Fix (#204)([7144209](https://github.com/madmax983/autumn/commit/7144209dd098b1d2db3e14370f52ceed3df4fa87))

### Wasm

- Fix cookie access, add prelude and wasm tests, and make target-specific dev-deps (#112)([bb49d40](https://github.com/madmax983/autumn/commit/bb49d405d64a498e813f21684fd35e335b368e7d))
## [0.1.0] - 2026-03-26

### Added

- Add Cargo feature flags for optional dependencies (S-044)([f6207c9](https://github.com/madmax983/autumn/commit/f6207c937dd19a7bf3402829a40fdde54b6d257d))
- Add E2E integration test for scaffolded project (S-037)([c09049f](https://github.com/madmax983/autumn/commit/c09049f535a34a4c14e20a0f97c334617e98ff27))
- Add todo-app example with Diesel, Maud, htmx, and Tailwind (S-041)([72e8a89](https://github.com/madmax983/autumn/commit/72e8a8987258672ae54f65e93942bcbedb89261a))
- Implement `autumn setup` — Tailwind CLI download with checksums (S-036)([56af096](https://github.com/madmax983/autumn/commit/56af0968379e370d739c1139e1c41de3726bd4f9))
- Add autumn-cli with project scaffolding and CI (Sprint 9)([2dc8314](https://github.com/madmax983/autumn/commit/2dc8314d3cd892bc6ddf5b00aadde579222cedd6))
- Expand env var overrides to all config fields (S-027)([c7a7782](https://github.com/madmax983/autumn/commit/c7a7782e4f1ef551771407cc6b97b2d8540c16d9))
- Add autumn::prelude module with common re-exports (S-033)([e0e9166](https://github.com/madmax983/autumn/commit/e0e9166670d7a00a1d7e90c6ffa218d571755e86))
- Add SIGTERM handling and shutdown timeout (S-030)([c30fe29](https://github.com/madmax983/autumn/commit/c30fe29a2633cac8ff27a0dc9338771c3d2fdc4c))
- Add health check endpoint with pool status (S-029)([e0c4a87](https://github.com/madmax983/autumn/commit/e0c4a877590c27ece3e8e3d77473f7f1d74650c4))
- Add structured logging via tracing-subscriber (S-028)([a2a40a5](https://github.com/madmax983/autumn/commit/a2a40a5b570624fb95d32064f55064bba163d2ac))
- Add static directory serving via tower-http ServeDir (S-032)([3ccb8a9](https://github.com/madmax983/autumn/commit/3ccb8a9ee10883e99e5c8216eb5c80bfcaea0ee3))
- Embed htmx 2.0.4 and serve at /static/js/htmx.min.js (S-022)([6e51ae9](https://github.com/madmax983/autumn/commit/6e51ae91d2c17a15fd5ffaee7cf463dc4e6c7419))
- Add Tailwind build.rs template and input.css (S-024, S-021)([d5053e2](https://github.com/madmax983/autumn/commit/d5053e25c1e40960cc87fba0f680eb72aa253895))
- Sprint 6 — Db extractor, Maud, Json, Form re-exports (S-017, S-020, S-023, S-031)([0b917ac](https://github.com/madmax983/autumn/commit/0b917acdab24b229a157b66a0f9ac297362d7961))
- Sprint 5 — database pool, #[model] macro, env config overrides (S-016, S-018, S-019)([e28b3fd](https://github.com/madmax983/autumn/commit/e28b3fd22a7d6afe5780ec6594e01846263cec99))
- Sprint 4 — error handling, macro diagnostics, request ID (S-007, S-012, S-011)([04c96bd](https://github.com/madmax983/autumn/commit/04c96bd899c126d7e74087e78928b45ee496b522))
- Sprint 3 — first running Autumn server (#4)([11bb094](https://github.com/madmax983/autumn/commit/11bb09468a190868064e81e8de4a28da6712e5ec))
- Implement routes![] collection macro (S-005)([efc1590](https://github.com/madmax983/autumn/commit/efc15900dd002441fd3517c15e1fdf9e6d5a0d07))
- Add #[post], #[put], #[delete] macros and debug_handler tests (S-003, S-004)([34e80f3](https://github.com/madmax983/autumn/commit/34e80f39e166b5cd1980ffac7934ea69a92ec560))
- Add TOML config file loading with ConfigError (S-026)([41b9573](https://github.com/madmax983/autumn/commit/41b9573cd7d65318402bce3920875136bc740d77))
- Add AutumnConfig struct with serde defaults (S-025) (#2)([4dda5bd](https://github.com/madmax983/autumn/commit/4dda5bd23d6dc132c8623fda5ab8fb64100139bd))
- Implement #[get] route macro with compile-fail tests (S-002) (#1)([66097a9](https://github.com/madmax983/autumn/commit/66097a9808bec4b14b16f08a8fa7a74ad0765052))
- Initialize workspace skeleton with autumn and autumn-macros crates (S-001)([604c348](https://github.com/madmax983/autumn/commit/604c3484286dc1bf4c8096cf9207eb3404c2893d))

### Fixed

- Resolve workspace-root DX issues and polish todo-app UI([d0d45ab](https://github.com/madmax983/autumn/commit/d0d45abf08df288782704bdd24f2f5e113a3dafb))
- Gate maud re-exports behind feature flag in API docs([84d8623](https://github.com/madmax983/autumn/commit/84d862371f4b71a0a009fdad05bc6e1c758b507e))
- Tailwind sha([26bb78f](https://github.com/madmax983/autumn/commit/26bb78f3a918fe53daffa87ac13a4096e3a06384))
- Add reason to #[ignore] attribute (clippy pedantic)([8f70857](https://github.com/madmax983/autumn/commit/8f70857fb64f4b252d53efdd86c2d364a8006101))
- Address code review — .pretty() format, stale doc, test gaps([b229019](https://github.com/madmax983/autumn/commit/b2290196ccbbdbbb64acc81a2dd9a7f895409c16))
- Address code review — explicit Response type, route priority test([209528a](https://github.com/madmax983/autumn/commit/209528a50fa99bd4bc0dc77fc4d9dd02db292795))

### Changed

- Simplify code quality across framework and example app([d28c3b3](https://github.com/madmax983/autumn/commit/d28c3b385cdf3eb6b58a4e3d535d8eccd4a9e130))
- Rename lib identity from autumn to autumn_web([a77a6d0](https://github.com/madmax983/autumn/commit/a77a6d0305fbc1c2b8b62641c3b6f671aa4ae43b))
- Publish as autumn-web on crates.io, keep autumn as lib name([3eb1ae7](https://github.com/madmax983/autumn/commit/3eb1ae7a13574fb7afe976213342d314ec6c4199))

### Documentation

- Add CI, coverage, license, and MSRV badges to README ([bc2eb3a](https://github.com/madmax983/autumn/commit/bc2eb3a4354b386a0ee2ff02745fd83166ff087c))
- Add Sprint 12 story (S-045) and update sprint status([370da00](https://github.com/madmax983/autumn/commit/370da0090e33ff8b2ea96eb2bac6f644f0161f39))
- Add Sprint 11 story definitions and update sprint status([2def24f](https://github.com/madmax983/autumn/commit/2def24fed07da9ac60cf7c5de14c3ce12cd50835))
- Add comprehensive API docs with examples on all public types (S-042)([dc894cd](https://github.com/madmax983/autumn/commit/dc894cd793b55d3fcb13bc7b0cc5cbb12f67541e))
- Add tutorial outline and Chapter 1 — Project Setup (S-040, Sprint 11)([c79b58b](https://github.com/madmax983/autumn/commit/c79b58b68c6ada0717f95549ab36f2b79a4ac6f5))
- Add getting started guide — zero to running app (S-039)([ae41763](https://github.com/madmax983/autumn/commit/ae41763b0afd37e913a0ea00139cdca4f89ea63b))
- Add README with quickstart and maturity warning (S-038)([1ac6798](https://github.com/madmax983/autumn/commit/1ac6798fc824ab173d593c42d429bb33c3daecb8))
- Add story documents for Sprint 10 and update sprint status([8b48585](https://github.com/madmax983/autumn/commit/8b485855ce61187ec742611953eacfc60f6146fc))
- Add story documents for Sprint 8 and update sprint status([f8b72cd](https://github.com/madmax983/autumn/commit/f8b72cd9c4f299d38225fd95273ff840768d7ee5))
- Add story documents for Sprint 7 and update sprint status([9dc1868](https://github.com/madmax983/autumn/commit/9dc1868ab614998f7c3bcfcdab0673cdd8b1f3bf))
- Add story documents for Sprint 6 and update sprint status([ed9d59a](https://github.com/madmax983/autumn/commit/ed9d59a174f26ed31c74989668e2d8f7b9b6abfb))
- Add story documents for Sprint 5 and update sprint status([41a396f](https://github.com/madmax983/autumn/commit/41a396feade34ec3235091db0c784ba056128bdf))
- Add story documents for Sprint 2 (recreated) and Sprint 3([56ac775](https://github.com/madmax983/autumn/commit/56ac775dc1bb0ef85a165f698c15067e0433e949))

### Testing

- Boost coverage from 84% to 91% on framework crate([33f410b](https://github.com/madmax983/autumn/commit/33f410b14ccf4cf21676111388626b392c21b2c5))
- Add missing spec-required tests for htmx serving and static 404([261a4a3](https://github.com/madmax983/autumn/commit/261a4a3b024d00d78fa543f4fd518236b4624f0e))

### Miscellaneous

- Commit CHANGELOG.md back to trunk on release([6b5eb82](https://github.com/madmax983/autumn/commit/6b5eb82b27d3932880f21b3cc3afc0fc29fa8790))
- Add codecov, dependabot, and changelog tooling for v0.1 (#9)([db0d670](https://github.com/madmax983/autumn/commit/db0d6705c6379880fd51c48ae728824530cce5cb))
- Update sprint status — Sprint 2 complete (13/12 pts)([07e0738](https://github.com/madmax983/autumn/commit/07e07387190401f4208f4a3eca1298bcaef5e856))

