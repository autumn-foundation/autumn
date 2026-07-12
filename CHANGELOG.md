# Changelog

All notable changes to the Autumn framework will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **config:** Autumn now auto-loads a project-root `.env` file, feeding its
  values into the highest (`AUTUMN_*` environment-variable) configuration layer.
  It is not a new precedence tier: a real shell environment variable of the same
  name always wins, so `.env` only fills keys that are still unset. Files load in
  order `.env` → `.env.local` → `.env.{profile}` → `.env.{profile}.local`, and
  earlier files (and real env vars) win. Auto-loading runs in the `dev` and
  `test` profiles and is skipped in `prod` unless `AUTUMN_DOTENV=1` (conversely
  `AUTUMN_DOTENV=0` disables it anywhere). A malformed `.env` fails loudly at
  startup rather than being silently ignored. `autumn new` now scaffolds a
  documented `.env.example` and gitignores `.env`, `.env.local`, and
  `.env.*.local` (while keeping `.env.example` and committable `.env.{profile}`
  files tracked). `autumn doctor` gained a `dotenv` check that warns when
  `.env.example` is present without a `.env`, or when a `.env` exists but is not
  gitignored (issue #1051). The environment/profile selectors (`AUTUMN_ENV`,
  `AUTUMN_PROFILE`, `AUTUMN_IS_DEBUG`) are intentionally NOT read from `.env` —
  a `.env` file must not be able to switch the active profile, so it can never
  flip `autumn migrate down` / `autumn db drop` / `autumn db reset` onto a
  production target after their real-env (dev) guards have already passed; set
  those in your shell or via `--profile`. The `dotenv` doctor check also
  surfaces an ungitignored secret file even when `.env.example` exists without a
  `.env`, so the copy-the-template hint can no longer hide a committable
  `.env.local`.
- **web:** new public `ProvideAuthorizationState` trait (PR #1505) [no-plugin]
  — the authorization layer (policy registry lookup, auth session key,
  forbidden response, and the `db`-gated connection pool accessor) is now
  driven through this trait instead of concrete `AppState`, decoupling
  `authorization.rs` from `state.rs`. `AppState` implements it, so existing
  apps are unaffected; custom state types can implement it to plug into
  authorization.
- **channels/sse:** Resumable SSE streams — automatic per-topic event ids, a bounded per-topic replay ring buffer, and `sse::stream_resumable` that replays events a client missed during a brief disconnect via `Last-Event-ID` (with a `gap` sentinel on buffer overflow) (issue #1356).

- **test:** first-class auth helpers for the test harness (issue #1359).
  `TestClient` now carries a cookie jar that persists each response's
  `Set-Cookie` and replays it on later requests from the same client, so a real
  `POST /login` → `GET /dashboard` flow works with no manual header threading.
  `TestClient::acting_as(user_id)` (alias `login_as`) establishes an
  authenticated session directly — writing the app's configured
  `auth.session_key` (default `user_id`) — so a `#[secured]` / `Auth` route
  returns its real success status without calling the login endpoint;
  `log_out()` clears the session cookie so secured routes reject again.
  `acting_as` sets identity only — policies/roles/scopes still run.
- **test:** built-in background-job recorder for the test harness (issue #1380).
  Every `TestApp::build` client now captures each enqueue — across `enqueue`,
  `enqueue_after_commit`, and `enqueue_in_tx` — as `(name, payload)` with no
  `with_job_interceptor` boilerplate. Assert with
  `TestClient::assert_job_enqueued`, `assert_job_enqueued_with`,
  `assert_no_jobs_enqueued`, or read them back in order via `enqueued_jobs()`.
  `perform_enqueued_jobs().await` drains the captured queue and dispatches each
  job through its registered handler, returning a `PerformedJobs` report that
  surfaces per-job handler errors (including malformed payloads that fail the
  real deserialization round-trip) rather than swallowing them. The recorder is
  a per-`TestApp` instance and composes ahead of any user-supplied
  `with_job_interceptor`.
- HTTP `Range`/`206 Partial Content` support for `Download` responses and
  embedded static assets (seekable media, resumable downloads) via the new
  `autumn_web::range` helper — RFC 7233 single-range parsing with a documented
  multi-range single-range collapse, `Accept-Ranges`/`Content-Range`/`416`
  handling, `If-Range` (strong `ETag` or `Last-Modified`), and
  `Download::into_response_ranged`; blob ranges fetch only the requested slice
  on the local store (S3/other backends fall back to a buffered slice) via the
  additive `BlobStore::get_range`.
- **widgets:** three new server-rendered, accessible, zero-JavaScript view
  helpers in `autumn_web::widgets` (all prelude re-exported, with `/_stories`
  gallery entries). `toast(message, variant)` / `toast_region(id)` /
  `toast_in(region_id, …)` render transient, CSS-auto-dismissing htmx action
  feedback: the toast appends into a fixed, persistent `aria-live="polite"`
  region out-of-band via `hx-swap-oob="beforeend:#<region-id>"`, reusing the
  shared `AlertVariant` color lane (`Error` announces assertively via
  `role="alert"`; non-error toasts inherit the region's politeness) — no
  `<script>`, no new color vocabulary (issue #1320).
  `infinite_feed(items, next_cursor, &FeedConfig)` + the companion
  `feed_page(items, next_cursor, &FeedConfig)` render an htmx infinite-scroll /
  "Load more" feed driven by a `CursorPage`: a single `hx-get` sentinel carries
  the cursor and appends the next page in place (no reload, no duplicate rows),
  in reveal (`hx-trigger="revealed, click"`) or explicit-button mode, with a
  progressive `<a href>` fallback (issue #1372). `tabs(id, panels)` completes
  the trio as the no-JS `tablist`/`tabpanel` switcher (issue #1316). Semantic
  `.autumn-toast*` / `.autumn-feed*` classes backed by the shipped `WIDGETS_CSS`
  stylesheet; all caller input HTML-escaped by Maud.
- **fuzzing:** coverage-guided fuzz harness (cargo-fuzz / libFuzzer) over the
  untrusted request-path parsing surface — idempotency-key, routing, headers,
  session-cookie, and body decoders — wired into CI as a per-PR crash gate
  (`fuzz.yml`, 30s/target seeded from the committed corpus) plus a nightly
  corpus-persisting long-run (`fuzz-nightly.yml`, 300s/target); crash
  reproducers upload as artifacts and the triage contract is documented in
  `CONTRIBUTING.md`. Developer tooling with no agent-facing framework surface
  (Closes #1637). [no-plugin]
- **testing:** property-based (proptest) invariant tests for the parser/codec
  surfaces, asserting round-trip and well-formedness invariants alongside the
  fuzz harness. Test-only, no public API change. [no-plugin]
- **mail:** automatic CSS inlining for HTML email (issue #1254). HTML bodies
  authored with a `<style>` block and CSS classes are transformed at send time
  so matching elements carry equivalent `style="…"` attributes in the delivered
  message — the fix for Gmail/Outlook stripping `<head>`/`<style>`. Opt in per
  message with `MailBuilder::inline_css(true)` or default it per environment via
  `mail.inline_css = true` (`MailConfig::inline_css` / `MailerBuilder::inline_css`);
  an explicit builder call wins over the config default in either direction, and
  the default is off so existing apps are unaffected. Un-inlinable `@media`/
  pseudo-class rules are preserved in a retained `<style>` block, text parts and
  bodies with no `<style>` pass through unchanged, and inlining is idempotent.
  Backed by the mature `css-inline` crate (gated behind the `mail` feature, with
  its network/file stylesheet fetchers disabled — only embedded `<style>` CSS is
  inlined). `autumn generate mailer` now scaffolds a `<style>`-block template and
  a mailer that calls `.inline_css(true)`, demonstrating the happy path end to
  end. On inliner failure `send` fails loudly, returning a typed
  `MailError::CssInline` instead of delivering a corrupted body — the message is
  not sent, so the body is never silently corrupted. Deferred/durable-queue
  sends (`deliver_later`) freeze the originating mailer's inlining default onto
  the message before it is persisted, so a worker consuming the queue with a
  different default still honors the sender's decision (explicit per-message
  overrides are preserved). The dev mail preview UI runs the same inlining pass
  as `send`, so previews of `<style>`-block templates show the inlined
  `style="…"` bodies strict clients actually receive rather than raw CSS.
- **generator:** `autumn new` now generates a `README.md` at the project root
  (listed in the "Created …" output) with explicit prerequisites and a
  golden-path quickstart — configure the `[database]` block in `autumn.toml`
  (the base scaffold ships it commented out, so `autumn migrate` would otherwise
  exit with "No database URL found"), then `autumn migrate` → `autumn dev` to a
  `200` on the default route — plus one-line descriptions of the most useful CLI verbs
  (`dev`, `migrate`, `doctor`, `routes`, `generate scaffold`, `release init`).
  The README is flag-aware: `--with-i18n` and `--with-seed` add sections for the
  extra steps they introduce (issue #1052). The DB-bootstrap step bootstraps a
  throwaway local Postgres with a copy-paste `docker run … postgres:16` one-liner
  that matches the generated `url`; this runnable helper lives in exactly one
  place (the "Configure the database" step), with the prerequisites section
  cross-referencing it instead of repeating the command — a top-to-bottom reader
  no longer starts the `…-pg` container twice and dead-ends on a
  container-name-in-use error (the earlier `autumn release init --target
  docker-compose` pointer file-errored on a fresh scaffold, which already ships a
  `Dockerfile`/`.dockerignore`, before any compose file was written; that pointer
  is retained for generating deployment/compose assets). After the `docker run`
  the README now waits for Postgres to accept connections
  (`until docker exec …-pg pg_isready …; do sleep 1; done`) before `autumn db
  create`/`autumn migrate`, since first-time container initialization takes a few
  seconds and those commands connect immediately without retrying. The golden path is also
  tailored to the generated app shape: `--daemon` scaffolds a database-free
  `autumn serve` app, so its README drops the Postgres/`libpq`/`autumn migrate`
  steps; `--bundled-pg` embeds and manages its own Postgres, so its README runs
  via `autumn serve --bundled-pg` and notes migrations apply automatically rather
  than telling users to configure an external `[database]`.
- **test:** opt-in channel broadcast recorder for integration tests (issue
  #1043) — `TestApp::record_broadcasts()` installs a recorder through the
  existing `ChannelsInterceptor` seam (no hand-written spy or `Arc<Mutex>`).
  After a request runs, read captured publications in order with
  `TestClient::broadcasts()` / `broadcasts_on(topic)` (both raw `publish` text
  and `publish_html` HTML/OOB payloads are captured), or assert on them with
  `assert_broadcast(topic, predicate)`, `assert_broadcast_count(topic, n)`, and
  `assert_no_broadcasts(topic)` — each failure self-diagnoses by listing what
  *was* published to that topic and nearby topics. The recorder is in-memory,
  ordered, thread-safe, and scoped to the `TestClient` (no global state, no
  cross-test leak); nothing is installed and production `Channels` behavior is
  untouched unless the builder is called.
- **generator:** `autumn generate scaffold` now emits **write-path CRUD tests**,
  not just a read smoke test (issue #1127). Alongside the in-process index/read
  test, the generated `tests/<snake>.rs` gains a `<plural>_write_path_crud`
  `#[tokio::test]` that drives create / update / delete **and** the
  validation-failure re-render through `autumn_web::test::{TestApp, TestClient}`:
  a valid `POST` redirects (303 See Other) and the row is observable on a
  follow-up read, an invalid `POST` re-renders the form at 422 with the
  submitted input preserved and an inline `role="alert"` error (and does not
  persist), an update is observable on re-read, and a delete removes the row.
  It runs fully in-process on the shipped `ChangesetForm`/`Changeset`
  round-trip (issue #1124), the typed `text_input` renderer, and `Redirect`
  against a process-local in-memory store — no database, no running server, no
  external services, so it is a visible green (never `#[ignore]`d) with real
  failure power (row-count assertions on each read turn a broken handler red).
  `TestApp` disables CSRF, so the same-origin form `POST`s carry no `_csrf`
  token — the real `form_for`-rendered forms (PR #1587) inject one for the
  browser, which the in-process harness does not require. Emitted for HTML
  scaffolds only (the `--api` JSON path is out of scope).
- **cli:** `autumn i18n check` (issue #1252) — a read-only diagnostic that
  compares the translation keys referenced in code (string literals passed to
  `t!(...)`, `.t(...)`, and `.t_with(...)`) against the keys defined in each
  `i18n/<locale>.ftl`, so a missing or untranslated string is caught in CI
  instead of from a production `Bundle::miss_count()` warning. It loads the
  bundle through the existing `Bundle::load_from_dir` loader and reports, per
  locale, **Missing** keys (referenced in code but absent from that locale's
  resolved fallback chain), **Untranslated** keys (defined in the default locale
  but resolving all the way to it for this locale — neither the locale itself nor
  any non-default locale in its fallback chain supplies them, so the user sees
  default-language text; a key an intermediate parent locale like `pt` supplies
  for `pt-BR` is not flagged), and **Unused** keys (defined in a `.ftl` with no call
  site). Exit is non-zero when any locale has Missing keys; Untranslated/Unused
  are warnings that become errors under `--strict`. `--format json` emits a
  machine-readable report for `autumn check`/CI to consume. Dynamically-built
  keys (e.g. `t(&format!(...))`) are listed as "dynamic — not checked" rather
  than silently ignored or falsely flagged. The i18n config is resolved through
  the same profile-aware loader the runtime uses (`AutumnConfig::load_with_env`),
  so `AUTUMN_ENV` and `[profile.<env>.i18n]` / `autumn-<env>.toml` overlays are
  honored — under `AUTUMN_ENV=prod` the check inspects the production locale
  directory and `supported_locales` instead of the base defaults, so missing
  production translations are no longer silently passed. A missing locale
  directory only skips (exit 0) when the project has *no* i18n configuration at
  all; when i18n *is* configured (a base `[i18n]` table or a
  `[profile.<env>.i18n]` / `autumn-<env>.toml` overlay for the active profile)
  but the resolved directory is absent, the check loads through
  `Bundle::load_from_dir` and fails with the same `MissingDefaultLocale` error
  the app would hit at startup, so CI no longer passes an app that cannot start.
  A translation call nested in another call's arguments (e.g.
  `t_with("message", &[("status", &locale.t("status.open"))])`) now has *both*
  keys recorded — the scanner recurses into the outer call's argument group — so
  removing the inner key from every `.ftl` is correctly reported as Missing
  instead of slipping past with exit 0. The scanner's intentional heuristic
  limits — the key-expression shapes treated as *dynamic — not checked* rather
  than validated — are documented under "Known heuristic limits" in the command
  module rustdoc and the plugin skill doc. See `autumn-cli/src/i18n.rs`.
- **generator:** `autumn generate controller <name> <action>...` scaffolds a handler-only module (named actions, wired routes, Maud stub views) for non-CRUD pages/endpoints — no model, migration, or DB; `--api` emits JSON actions. (issue #1050)
- **download:** typed `Download` `IntoResponse` (`autumn_web::download::Download`)
  for serving files from a handler without hand-rolling headers. Construct it
  from owned bytes, an async byte stream, an `AsyncRead`, or a stored blob
  (`Download::from_blob(&store, key).await?`), then chain `.filename(...)`,
  `.content_type(...)`, and `.inline()`. It sets `Content-Disposition`
  (RFC 5987-encoded for non-ASCII names, sanitized against header injection),
  infers `Content-Type` from the filename extension (or blob metadata, falling
  back to `application/octet-stream`), and sets `Content-Length` when the size
  is known. The blob-backed path streams via the new
  `BlobStore::get_stream` without buffering the whole object in memory, so it
  serves large private files behind a `#[secured]` handler with no public
  presigned URL (#1141).
- **lock:** app-facing distributed lock for run-once-across-replicas work
  (issue #1387) — `autumn_web::lock::Lock` promotes the Postgres advisory-lock
  machinery that already gates migrations, `#[scheduled]` leader election, and
  ISR revalidation into a small, safe public API. `Lock::new(pool, "name")`
  (or `Lock::from_state(&state, "name")`) hashes the name to a stable,
  collision-namespaced 64-bit key (kept out of the scheduler/migration/ISR/
  repository keyspaces via a `"autumn:lock:v1"` domain prefix, see
  `distributed_lock_key`) and offers both a blocking `lock` / `lock_timeout`
  (typed `LockError::Timeout` on expiry) and a non-blocking `try_lock`
  (returns `None` immediately when another node holds it), plus `with` /
  `with_timeout` / `try_with` closure wrappers that auto-release the lock when
  the guarded section ends — on normal return, an early `?`, or a panic. While
  held, the lock keeps its connection as a checked-out pooled connection
  (counted against `database.pool.max_size`, never returned to the shared pool
  while held); a clean release runs `pg_advisory_unlock` and recycles it, while
  panic/cancel/unlock-error paths force-close the session — so holding many
  locks stays bounded by the pool and a recycled lock-bearing connection can
  never leak the lock. `lock_timeout` also bounds the initial pool checkout by
  the deadline, so a small timeout returns `Timeout` on time under pool
  pressure. The `bookmarks-distributed` link-checker was rewritten onto the
  primitive, deleting its hand-rolled `pg_try_advisory_lock` /
  `pg_advisory_unlock` raw SQL. `Lock`, `LockGuard`, and `LockError` are
  re-exported from the prelude. See `docs/guide/distributed-locks.md` and
  `docs/adr/0010-app-facing-distributed-lock.md`.
- **mail:** bounce/complaint suppression list (issue #1247) — closes the
  detect→suppress loop so a sending domain's reputation survives contact with
  real recipients. New `autumn_web::mail::suppression` module ships a
  `SuppressionStore` trait (`is_suppressed(addr)`, `suppress(addr, reason)`,
  `unsuppress(addr)`) with a `SuppressionReason` enum (`HardBounce`,
  `Complaint`, `Manual`), an `InMemorySuppressionStore` zero-config default,
  and a `db`-feature `PgSuppressionStore` (a `mail_suppressions` table) for
  multi-instance deploys — mirroring the memory/durable split used by sessions
  and jobs. `Mailer::send()` now consults the store **before** transport:
  suppressed recipients are skipped (not an error), each skip emits a
  structured `outcome = "skipped_suppressed"` log line and bumps the
  process-wide `suppression::suppressed_skips()` counter, and when *every*
  recipient is suppressed `send()` returns the new
  `MailError::AllRecipientsSuppressed` rather than a phantom success. Critical
  mail opts out per message with `Mail::builder().ignore_suppression()`
  (password resets, MFA codes, security alerts). The provided
  `suppression::record_inbound` handler turns a parsed provider bounce into a
  suppression entry in one call from the inbound router's `on_bounce` hook,
  using the provider-reported `InboundEmail::bounced_address` (never `email.to`,
  the app's own inbound address). It suppresses a complaint only from a genuine
  FBL complainant in the new `InboundEmail::complained_address`; autumn's
  `on_spam` is an inbound spam *verdict* (not an outbound complaint) and is a
  logged no-op there, so an attacker cannot POST the inbound endpoint to force
  addresses onto the outbound suppression list. Register a durable backend with
  `AppBuilder::with_mail_suppression_store(...)`; the in-memory default is
  auto-wired otherwise. **`PgSuppressionStore` ships no migration — you must
  create the `mail_suppressions` table** (`address TEXT PRIMARY KEY, reason TEXT
  NOT NULL, suppressed_at TIMESTAMPTZ NOT NULL DEFAULT now()`), matching the
  List-Unsubscribe store convention. See `skills/autumn-web/SKILL.md`.
- **actuator:** `PUT /actuator/loggers/{name}` now changes the **live**
  `tracing` subscriber, not just an in-memory map (issue #1044). The default
  telemetry init installs a `tracing_subscriber` reload layer and hands the
  handle to `LogLevels`; a level change rebuilds the combined `EnvFilter`
  directive (global level + per-target overrides) and pushes it to the running
  subscriber, so an operator can raise/lower verbosity in production without a
  redeploy — effective on the next event. Per-target overrides
  (`my_app::module=trace`) raise only that target; reverting `root` to `info`
  silences them again. Overrides stay ephemeral (reset on restart). The
  response reports `"applied": true` / `"status":"ok"` only when the change
  actually reached a reload-capable subscriber, and `"status":"recorded"` /
  `"applied": false` otherwise — no silent false-positive. Invalid levels still
  return `400`. A startup `log.level` that is a full `EnvFilter` directive
  (e.g. `"info,tower_http=warn,my_app=debug"`) now seeds its per-target
  segments into the override map at construction, so changing the `root` level
  at runtime no longer drops the module-specific directives configured at
  startup.
- **actuator:** `GET /actuator/info` now exposes build + git provenance for
  deploy/rollback verification (issue #1242): a `build` object with the app
  version, an ISO-8601 UTC build timestamp, and a `git` sub-object (full +
  short commit SHA, branch, working-tree-dirty flag). Apps created by
  `autumn new` capture this with zero developer action — the generated
  `build.rs` bakes `AUTUMN_BUILD_*` env vars and `#[autumn_web::main]` reads
  them (plus the app's compile-time `CARGO_PKG_NAME` / `CARGO_PKG_VERSION`) at
  the app's compile time. This also fixes the `app.version` / `app.name`
  `"unknown"` regression (they were read from the cargo env at runtime, which
  is unset in a released binary). Outside a git checkout the `git.*` fields
  degrade to `null` while timestamp + version stay present; the block never
  leaks remote URLs or an env dump.
- **security:** per-route `#[throttle]` route attribute (issue #1350) —
  drop-in stricter rate limit for abuse-prone endpoints (login, search,
  export). `#[throttle(limit = 5, per = "1m")]` bounds requests to that
  handler on top of the global limiter, `#[throttle(limit = N, per = "…",
  key = "ip" | "principal" | "token")]` selects the keying strategy, and
  `#[throttle("login")]` references a named limiter defined in
  `[security.rate_limit.named.login]`. Reuses the shipped token-bucket
  backend (memory + Redis), the shipped principal/IP/token keying (#794),
  and the existing 429 `Retry-After` + `x-ratelimit-*` response shape.
  `RateLimitExempt` still bypasses per-route throttles, and backend errors
  honor `on_backend_failure` fail-open/fail-closed. See
  `docs/guide/rate-limiting.md`.
- **observability:** `Server-Timing` response header (issue #1348) —
  standards-conformant W3C `total;dur=…` plus
  `db;dur=…;desc="N queries"` roll-up so N+1s show up directly in the
  browser DevTools Network → Timing pane. Opt-in via
  `[observability] server_timing = true` (or
  `AUTUMN_OBSERVABILITY__SERVER_TIMING=true`); defaults on in
  `dev`/`development`, off everywhere else so prod never leaks timings to
  anonymous clients without explicit opt-in. `total` uses the identical
  clock formula as the access-log `duration_ms`; SSE
  (`text/event-stream`) responses receive `total`-only. MCP `tools/call`
  responses forward the dispatched handler's non-`total` `Server-Timing`
  metrics (including `db;dur;desc="N queries"`) onto the `/mcp` response, so
  DB-backed tool calls surface their query count — while the inner-dispatch
  `total` is dropped in favour of the outer fallback's real `/mcp` `total`,
  which brackets the endpoint's body buffering/JSON-RPC repackaging (the inner
  `total`, captured before that work, would under-report `/mcp` latency). See
  `docs/guide/observability/server-timing.md`.
- **conditional-get:** declarative per-handler `Cache-Control` freshness helper
  (issue #1344) — `cache_for(Duration)` builds a `CacheControl` that attaches
  `Cache-Control` to any response either as a tuple
  (`(cache_for(dur).public(), html!{…})`, via `IntoResponseParts`) or with
  `.wrap(response)`. Chainable directives: `public`/`private`, `max_age`,
  `s_maxage`, `stale_while_revalidate`, `no_store`, `no_cache`,
  `must_revalidate`, `immutable`; `header_value()` renders a deterministic,
  byte-for-byte value. Defaults to `private` so dropping it onto a
  secured/authenticated page can't silently make it publicly cacheable —
  `public` is an explicit opt-in. Composes with `fresh_when`
  (`fresh_when(&headers, etag).or(cache_for(dur).public().wrap(markup))`): the
  freshness directives ride along on both the `200` and the preserved `304`,
  emitting exactly one `Cache-Control` header. Both re-exported from the
  prelude. See `docs/guide/conditional-get.md`.
- **Atom/RSS feed renderer** (`feed::Feed` / `feed::FeedEntry`): build an Atom 1.0 or RSS 2.0 feed from channel metadata plus an iterator of entries and return it directly from a `#[get]` handler — it implements `IntoResponse` with the correct `application/atom+xml`/`application/rss+xml` content type, XML-escapes every text field, and `Feed::conditional(&headers)` reuses the `etag` layer so feed pollers get a `304 Not Modified` on unchanged content. The `blog` example gains a `/feed.xml` route. (#1045)
- **router:** duplicate-route preflight (issue #1012) — two user- or
  plugin-registered handlers that resolve to the same `(method, path)` after
  `.scoped(prefix, …)` prefix resolution now fail app build with a structured
  `RouterBuildError::DuplicateUserRoute` **before any router is mounted**,
  instead of an `axum::routing::MethodRouter::merge` panic at startup. The
  error names BOTH handlers, the HTTP method, and the resolved path. The
  synthetic `WS` method a `#[ws]` handler mounts as `GET` is normalized before
  keying, so `#[get("/live")]` + `#[ws("/live")]` is caught too. Two routes
  whose **different** templates resolve to overlapping path shapes are a matchit
  route conflict *regardless of HTTP method* (so `GET /users/{id}` +
  `POST /users/{slug}` clashes) and now fail with a dedicated
  `RouterBuildError::ConflictingRouteShape` that names BOTH handlers and BOTH
  original templates, instead of leaking an axum matchit panic. Path-shape
  conflict detection is delegated to **matchit** — the exact engine axum 0.8
  routes through — instead of a hand-rolled normalizer, so it mirrors axum's
  accept/reject behavior precisely on every edge case: capture-name diffs
  (`/users/{id}` vs `/users/{slug}`), catch-all vs sibling capture (`/u/{id}`
  vs `/u/{*rest}`), catch-all vs dynamic *descendant* (`/cmd/{tool}/{sub}` vs
  `/cmd/{*path}`, which the old normalizer missed), and mixed literal+capture
  segments (`/file.{ext}` vs `/file.{kind}`). Because the check *is* matchit it
  never over-flags what axum accepts: static-vs-capture (`/users/me` vs
  `/users/{id}`), escaped literal braces (`/{{foo}}` vs `/{{bar}}`), and mixed
  segments like `/file.{ext}` vs `/file.json` build cleanly. A permanent parity
  test pins matchit's verdicts to axum 0.8.9's so a future axum bump cannot let
  the oracle drift silently. Distinct methods on
  the *same exact path* (`GET /admin` + `POST /admin`, `GET /users/{id}` +
  `POST /users/{id}`) and genuinely different shapes (`/users/{id}` vs
  `/users/{id}/posts`) are unaffected;
  `#[repository]`-generated API routes are covered because they land in the
  normal `Route` list. Opaque `AppBuilder::merge` and `AppBuilder::nest`
  routers cannot be introspected — a non-empty opaque table emits a
  `tracing::warn!` ("check skipped") mirroring the existing OpenAPI/MCP
  merge-router warnings. See `docs/guide/getting-started.md`
  ("Route collision diagnostics").
- **migrations:** content checksums for applied migrations (issue #1203) —
  the framework now records a SHA-256 of every migration's `up.sql` in a
  new `autumn_migration_checksums` table (created by the framework
  migration `20260709000000_create_migration_checksums`) when it is
  applied via `autumn migrate run` or backfilled by `autumn migrate
  baseline`. Startup auto-migrate **validates** but does not record: it
  applies the embedded SQL compiled into the binary (which may differ
  from the on-disk files), so recording those disk bytes could store a
  hash for content that was never applied — recording is deferred to the
  CLI/baseline paths where applied bytes == on-disk bytes. Before every
  subsequent `autumn migrate` run and before startup auto-migrate, each
  applied migration's on-disk `up.sql` is re-hashed and compared against
  the recorded value; a
  mismatch fails fast with a message that names the version and both
  hashes: `migration <version> checksum mismatch: recorded <hex-a> but
  on-disk content hashes to <hex-b>. Migrations must never be edited
  after being applied — add a new migration instead, or run the
  documented re-baseline command if this change was deliberate.` Hashing
  normalises line endings (`\r\n`/`\r` → `\n`) and trims trailing
  whitespace so a Windows checkout and a Linux one produce identical
  checksums. `autumn migrate status` reports each applied migration's
  state (`ok`/`changed`/`unrecorded`), excluding framework-owned migrations
  (the same set rollback excludes) so operators are never prompted to
  `baseline` framework versions whose `up.sql` does not live in the user dir;
  `autumn migrate baseline` records
  hashes for legacy applied migrations that pre-date the checksum table
  (idempotent, additive); `autumn migrate baseline --force <version>`
  overwrites one version's stored hash — the deliberate escape hatch,
  WARN-logged. Both baseline paths run their applied-versions read and
  checksum write under the same advisory migration lock as `run`/`down`, so a
  concurrent rollback cannot revert a version between baseline's read and its
  write. Rolling a migration back (`autumn migrate down`) now clears its
  recorded checksum, so a reverted migration can be re-applied cleanly —
  including with changed contents — instead of leaving a stale hash that would
  trip the drift guard on a later run. `autumn migrate status` and the pre-apply
  validation are read-only — they never create the checksum table, so displaying
  or checking state needs no DDL privileges — and freshly-applied user
  migrations are recorded immediately after they apply (before the framework
  migration step), so a later framework failure can no longer leave them
  unrecorded and mask a subsequent edit. See `docs/guide/migrations.md`.
- **repository:** race-safe get-or-insert (#1382) — declaring
  `fn find_or_create_by_<field>[_and_<field>...](...)` in a `#[repository]`
  trait generates an inherent
  `find_or_create_by_<field>(&self, <field>: <Ty>, ..., new: &NewModel) ->
  AutumnResult<(Model, bool)>` that returns the model plus a `created` flag. It
  looks the row up on the read path first (replica-eligible, honoring tenant
  scoping and soft-delete); if absent it inserts on the primary with
  `ON CONFLICT DO NOTHING`, so under concurrent callers exactly one row is
  created, exactly one caller observes `created == true`, and no
  unique-violation (`23505`) is ever surfaced — a concurrent loser re-reads its
  own write on the primary and returns `(row, false)`. `before_create` /
  `after_create` and the durable commit-hook queue fire only on the created
  path, and — unlike `upsert_many` — the method is generated even on hooked
  repositories. Race-safety requires a unique constraint covering the lookup
  column(s); `_or_` is rejected because it would span constraints. On a
  sharded, tenant-scoped repository the generated method is wrapped in the same
  cross-shard write guard as `save`/`update`/`delete`, so a get-or-insert issued
  through `across_tenants()` is rejected rather than silently writing to a single
  shard while matching rows on other shards go unseen. See the
  "Race-safe get-or-insert" section of the repositories guide.
- **repository:** typed grouped aggregate queries (#1364) — declarative
  `GROUP BY` roll-ups on a `#[repository]` trait. **Before:** dashboard
  aggregates (a post's vote tally, an experiment's audit-trail size) were
  hand-written raw `diesel::sql_query("SELECT … SUM/COUNT … GROUP BY …")`
  strings that bypassed the repository's replica routing, tenant scoping, and
  soft-delete filters and had to be re-typed for every widening cast.
  **After:** declare the aggregate by method name with its pair return type —
  `count_grouped_by_<col>() -> Vec<(K, i64)>` or
  `sum_/avg_/min_/max_<num_col>_grouped_by_<col>() -> Vec<(K, Option<T>)>`
  (`avg` rolls up to `Option<f64>`) — and the macro generates an inherent
  method returning a lazy `GroupedAggregate<'_, K, V>` builder that yields one
  `(group, aggregate)` pair per group. Chain `.order_by_aggregate_desc()` /
  `.limit(n)` for top-N, `.filter_eq(v)` / `.filter_range(lo, hi)` to scope the
  group column *before* aggregating, or `.bucket(DateBucket::{Day,Week,Month})`
  for a `date_trunc` time series, then `.load().await`. Filter values are bound
  as parameters (never interpolated); the query composes the same soft-delete +
  tenant predicates as `count` and acquires its connection through the read
  route, so replica routing and multi-tenancy come for free.
  `sum`/`avg`/`min`/`max` are null-safe (an all-`NULL` group yields `None`, an
  empty table an empty `Vec`) and are rejected on a sharded, tenant-scoped
  repository used via `across_tenants()` rather than returning a
  per-shard-partial answer. The reddit-clone vote tally and the admin
  experiment-history count now use this instead of raw `SUM`/`COUNT` SQL.
  Closes #1364. See "Grouped aggregate queries" in
  `docs/guide/repositories.md`.
- **widgets:** `flash_messages(&[FlashMessage])` (issue #1240) — an accessible
  renderer for consumed flash messages. Each banner is its own live region
  whose `role`/`aria-live` is chosen by severity (`Error`/`Warning` announce
  assertively, `Success`/`Info` politely), carries semantic
  `autumn-flash`/`autumn-flash--<level>` classes backed by `FLASH_CSS`, and
  escapes its text. An empty slice renders nothing; `flash_messages_with` adds
  an opt-in, no-JavaScript dismiss control. The `flash` module doc now points
  at the helper instead of the hand-rolled `div class=(level)` snippet.
- **widgets:** `badge`/`status_tag` (issue #1259) — semantic status pills.
  `badge(label, BadgeVariant)` emits a stable `badge badge--<variant>` class
  (`Neutral`/`Info`/`Success`/`Warning`/`Danger`), `BadgeVariant::for_label`
  maps an arbitrary status string to a deterministic color, `status_tag` is the
  neutral one-liner, and `badge_with`/`BadgeConfig` set a `title`/`aria-label`.
  Text is always present (color is never the sole signal); no inline styles.
- **widgets:** `avatar(name, &AvatarConfig)` (issue #1263) — renders an `<img>`
  (lazy-loaded, square `width`/`height`, name-derived `alt`) when an image URL
  is present, or a deterministic colored-initials badge when it isn't (1–2
  Unicode-safe uppercase initials, per-name background via a stable
  `autumn-avatar--cN` palette class — no inline `style`, so it survives a
  nonce-based CSP). Never a broken-image request; three named sizes
  (`Small`/`Medium`/`Large`); the display name is HTML-escaped.
- **widgets:** `alert`/`alert_with` + `error_summary` (issue #1314) — inline
  block-level callouts with an `AlertVariant` (`Info`/`Success`/`Warning`/
  `Error`, `role` chosen per variant), optional title, per-variant inline-SVG
  icon, and an opt-in no-JavaScript dismiss control. `error_summary(&Changeset)`
  renders an `Error` alert listing every field error as a `<ul>` (stable order)
  or `None` when valid, for the form re-render path. All caller markup is
  escaped by Maud; no inline styles.
- **model:** `#[private]` field attribute (issue #1374) hides a `#[model]`
  column from JSON — it is excluded from the model's `Serialize` impl so it
  never appears in `Json` output, the auto-generated `--api` list/show
  endpoints, or any `serde_json::to_value(&model)`, while staying a normal,
  queryable column whose write path (`NewX`/`UpdateX`/`Changeset`) still binds
  it (set a password, never read the hash back). `#[encrypted]` columns are now
  `#[private]` in JSON by default (opt back in via `#[encrypted(admin_visible)]`);
  `#[private]` still appears in `FormModel::form_fields()` (the write side). New
  `autumn doctor` check `model_private_columns` warns when a sensitively-named
  column (`password`/`token`/`secret`/`*_hash`) is not marked `#[private]`.
- **model:** `#[normalize(trim, downcase, upcase, squish, with = path)]` field
  attribute (issue #1379) canonicalizes a `String` column, composing
  normalizers left-to-right. Runs on the write path (`save`/`save_many` insert
  and `update` via `UpdateDraft::from_patch`) before the `before_create`/
  `before_update` hooks and the DB write, and on derived `#[repository]`
  `find_by_`/`count_by_` lookups (so `find_by_email("  FOO@X.com ")` matches the
  stored `foo@x.com` row). Built-ins are idempotent, so `#[normalize(downcase)]`
  plus a `unique` column gives case-insensitive uniqueness; non-`String` fields
  are a compile error. Built-ins and the `Normalize` / `NormalizedModel` traits
  live in the new `autumn_web::normalize` module.
- **ci:** README-quickstart gate against the published crates (issue #1586) —
  `.github/workflows/quickstart-gate.yml` + `scripts/check-quickstart.sh`
  install the README-pinned `autumn-cli` from crates.io (never the local
  workspace), run `autumn new` / `autumn setup` / build / serve and assert a
  200 from `GET /`, then run the README's
  `autumn generate scaffold Post title:String body:Text published:bool` path
  through build, `autumn migrate`, and a 200 from `GET /posts`. Runs on every
  push to `trunk-dev`, on a daily schedule (catches upstream dependency
  releases), and via `workflow_dispatch` with a `cli-version` input for gating
  a freshly published release candidate (post-publish, pre-announce — see
  `docs/release-checklist.md`). Each phase is a named CI step that emits
  `::error::` on failure so a red run names the broken quickstart step, and
  the job summary records the tracked install→first-200 funnel time.
- **generator:** `autumn generate tauri --remote-url <URL>` scaffolds a
  **mobile thin-client** Tauri shell (issue #1506): the webview loads your
  remote HTTPS Autumn server directly (https enforced; loopback and
  Android-emulator hosts exempt for dev), capability files grant exactly
  that origin access to the notification/biometric/store plugins
  (`capabilities/remote-app.json`, plus `remote-app-mobile.json` restricting
  the biometric grant to Android/iOS so desktop smoke-test builds still
  pass), and `autumn destroy tauri --remote-url <URL>` reverts the
  scaffold. Desktop `autumn generate tauri` output is unchanged. Generating
  one Tauri mode over the other's files is rejected (even with `--force`,
  which only overwrites within the same mode) with a pointer at the matching
  `autumn destroy tauri [--remote-url <URL>]` to run first — mixing modes
  would leave stale files that break the new scaffold's build. See
  `docs/guide/tauri-mobile-thin-client.md`.
- **views:** widget storybook (issue #1526) — a browsable gallery of every
  built-in maud widget plus a CI anti-rot harness. `autumn_web::stories`
  ships `Story`/`StoryRegistry`/`StoryGallery` (mirroring the mail-preview
  registry), a `story!{ group, name, { ... } }` macro whose block is **both**
  executed for the live render and captured byte-for-byte (comments and
  formatting included) as the displayed snippet — so the shown code is
  provably the code that rendered — and `stories::builtin()` with a story for
  every gallery-visible widget. Stories are zero-arg pure `fn() -> Markup`
  pointers: capturing a `Db` handle, `AppState`, or any local is a compile
  error. Mount with `.with_story_gallery(StoryGallery::builtin())` — routes
  at `GET /_stories` (grouped index) and `GET /_stories/{slug}` (live render
  + Source + Rendered HTML tabs, dogfooding the `tabs` widget and styled by
  the framework widget stylesheet) are **off by default** and opt in per
  profile via `[stories] enabled = true` (e.g. `[profile.dev.stories]` for a
  dev-only gallery, or a prod profile for a public showcase; `/_stories` is
  404 wherever the resolved flag is false; `AUTUMN_STORIES__ENABLED`
  overrides from the environment). Apps register their own widgets with the
  same `story!` macro via `StoryGallery::builtin().extend(...)` or a
  builtin-free `StoryGallery::new()`. CI renders every builtin story
  (panic-free, non-empty, balanced HTML, unique slugs) and a two-layer
  coverage gate fails the build when a widget in `widgets.rs` gains no story.
  See `docs/guide/stories.md`.
- **repository:** bounded-memory batched iteration (#1395) — every
  `#[repository]` now generates `find_in_batches(batch_size)` (yielding
  successive `Vec<Model>` chunks of at most `batch_size`) and
  `find_each(batch_size)` (yielding one `Model` at a time), the read-side
  companion to the bulk writes from #841. Iteration is primary-key keyset-based
  (`WHERE id > last ORDER BY id ASC LIMIT batch_size`), not `LIMIT`/`OFFSET`, so
  walking a million-row table in a `#[autumn_web::task]`, job, or sweep holds
  `O(batch_size)` models — never `O(table)` — and stays stable under concurrent
  inserts. Unlike a `cursor_page` request, `batch_size` is not clamped to
  `MAX_PAGE_SIZE`. The iterators reuse the same soft-delete filter and read
  routing as `find_all`/`cursor_page` (trashed rows are skipped; a
  replica-routed repo iterates off the replica), an error mid-iteration surfaces
  on the failing batch and is retryable (the keyset cursor only advances on
  success, so a retry resumes with no duplicated or skipped rows — `Ok(None)`
  always means completion), a
  `batch_size` of `0` errors rather than spinning, and sharded repositories
  reject cross-shard `across_tenants()` iteration exactly as `cursor_page` does.
  The generic handle types live in `autumn_web::batches`
  (`FindInBatches`, `FindEach`, `BatchSource`). See the "Batched iteration"
  section of the pagination guide.
- **form:** `autumn_web::form::form_for` — a model-driven form builder that
  renders a complete form in one call (issue #1135): opening `<form>` (hidden
  `_method` override for `PUT`/`PATCH`/`DELETE` + auto-injected CSRF, via the
  same audited path as `form_tag`), one type-appropriate control per field with
  values pre-filled and inline per-field errors, and a submit button. Controls
  come from a new `#[model]`-derived `FormModel`/`FormField`/`FieldControl`
  descriptor, reusing the #1131 typed inputs — no per-field control selection in
  caller code. Escape hatches: `.exclude`, `.override_field`, `.override_label`,
  `.append`, `.submit_label`, `.multipart`. Plain no-JS HTML; htmx stays opt-in.
  A required `FieldControl::Date` field now renders via a new
  `required_date_input` helper (`required` + `aria-required="true"`), matching
  the other `required_*` siblings. Public API addition (minor bump); existing
  helpers unchanged.
  `autumn generate scaffold` now consumes it: the generated create/edit views
  (and both 422 re-render branches) render through one shared
  `{snake}_form_for` helper instead of hand-emitting one input per column —
  the generated `{Model}Form` delegates `FormModel` to the `#[model]`-derived
  descriptors, enum columns get a `.override_field(...)` `Select` with their
  variants, decimal columns pin the browser `step` to the declared scale, and
  attachment columns stay a hand-rolled file input (appended before the submit
  button, so attachment fields now always render at the end of the form
  regardless of their declared column position, and the form remains
  URL-encoded) that renders the same inline-error/ARIA skeleton as the derived
  controls, so changeset errors on the attachment key surface next to the
  file input. `references` columns render as a `<select>` of the referenced
  table's ids (AC "enum/references→select"): the generated handlers load the
  ids via a per-column `{column}_select_options` loader and thread them into
  the shared form helper — option labels are the raw id (which column makes a
  human-friendly label is a display decision the generator can't know; the
  generated loader's doc comment says where to swap one in). The select needs
  the referenced resource's `src/schema.rs` entry, so it applies when the
  referenced model is already in the project (generate it first — the same
  ordering the FOREIGN KEY already imposes); when the target is missing (a
  warning-only situation: the table is assumed to exist out-of-band), the
  column falls back to the derived numeric id input so the generated code
  still compiles, and the scaffold warns that generating the referenced model
  first (or re-running the scaffold afterwards) yields the select.
  Adding a column no longer requires any view edits.
  `--live-validation` scaffolds keep the per-field emission (their htmx
  inline-validation inputs have no `FieldControl` equivalent).
  Non-nullable `bool` columns: the `#[model]`-generated `NewX` insert struct
  now marks them `#[serde(default)]`, so an unchecked `form_for` checkbox
  (which submits no key at all — `checkbox_input` deliberately emits no hidden
  `false` fallback because serde_urlencoded rejects duplicate keys) decodes as
  `false` instead of rejecting the submission with a missing-field error,
  matching the scaffold's `{Model}Form` convention. Side effect: a JSON create
  body that omits a non-nullable `bool` now also decodes it as `false` rather
  than erroring.
  Datetime columns: `form_for` renders `chrono::DateTime<Utc>` and
  `NaiveDateTime` columns as `<input type="datetime-local">`, whose submitted
  value carries no timezone offset (and not always seconds) — chrono's default
  `Deserialize` for `DateTime<Utc>` would reject even an untouched pre-filled
  value as a 400 before validation. The `#[model]`-generated `NewX` insert
  struct now attaches `autumn_web::form::deserialize_datetime_local_utc`
  (`_option` for nullable) to `DateTime<Utc>` columns and
  `deserialize_naive_datetime_local` (`_option`) to `NaiveDateTime` columns:
  the offsetless browser value is interpreted as UTC, an empty nullable value
  decodes as `None`, and RFC 3339 JSON create bodies keep decoding — the
  `DateTime<Utc>` helpers now also accept RFC 3339 input, honoring an explicit
  offset by converting to UTC. These four `form` deserializer helpers,
  previously `maud`-gated, are now available unconditionally.
  `chrono::DateTime<Local>` columns (diesel's other `Timestamptz`-capable
  zone) get the same treatment via new
  `autumn_web::form::deserialize_datetime_local_local` /
  `deserialize_datetime_local_local_option` helpers: the offsetless browser
  value is interpreted as the server's local wall clock (a wall clock
  repeated by a DST fall-back maps to the earlier instant; one skipped by a
  spring-forward is a decode error), and RFC 3339 input converts the instant
  to the local zone. `DateTime` columns with any *other* zone parameter
  (e.g. `FixedOffset`, or a bare `DateTime` alias hiding its zone from the
  derive) no longer render the `datetime-local` picker at all — an
  offsetless wall clock is genuinely ambiguous there, and the picker's
  submission used to 400 unconditionally; they fall back to a text input
  pre-filled with the serialized RFC 3339 string, which chrono's default
  `Deserialize` round-trips as-is. `UpdateX` is untouched: it has no `Validate` impl so no
  `ChangesetForm`/`form_for` round-trip reaches it, and its JSON PATCH bodies
  are RFC 3339, which already decodes. Required `NaiveDate` columns need no
  treatment (the browser's `YYYY-MM-DD` is exactly chrono's wire shape).
  Serde-renamed columns: a model field carrying `#[serde(rename = "...")]`
  (or covered by a struct-level `#[serde(rename_all = "...")]`, both of which
  pass through to the emitted model struct's `Serialize` derive) serializes
  under a key that differs from its Rust identifier, and `form_for`'s
  pre-fill lookup (`Changeset::field_value`) indexes serialized data — so
  edit forms for renamed fields rendered blank/unselected values despite the
  data being present. `FormField` now carries the serde-effective serialized
  key separately as a new `value_name: Option<String>` field (constructor
  defaults it to `None` = same as `name`; set via the new
  `FormField::with_value_name`), which the `#[model]` derive resolves
  automatically — field-level `rename`/`rename(serialize = ...)` wins over
  struct-level `rename_all`, mirroring serde, with all eight serde field
  casings implemented. `form_for` uses `value_name` **only** for the value
  pre-fill; the rendered input `name`/`id`, error lookup, and
  `.exclude`/`.override_*` matching keep the Rust identifier, which is what
  the generated `NewX`/`UpdateX` structs (which do not propagate serde
  renames) decode by. Hand-written `FormModel` impls over renaming data
  types should call `.with_value_name(...)` themselves. (`FormField` gains a
  public field; code constructing it via struct literal instead of
  `FormField::new` needs the extra field.)
  `FieldControl` is `#[non_exhaustive]` (new control kinds won't be
  semver-major); duplicate `.override_field`/`.override_label` calls on the
  same field now resolve last-wins; `FieldControl::File` renders the same
  inline-error/ARIA/required skeleton as the other controls; and the
  multipart render path now flows through the same audited CSRF/
  method-override code as every other form (`enctype` is threaded, not
  duplicated).

- **generator:** `autumn generate scaffold`'s `create`/`update` handlers now
  build a `Changeset` from the submission and re-render the `new`/`edit`
  form on a rejected submission (issue #1124). A failed submission responds
  **422** (not the old 400 error page) and re-renders through the shipped,
  accessible `autumn_web::form::{text_input, number_input, datetime_input,
  checkbox_input, select_input}` helpers — every submitted field value is
  preserved and per-field error messages appear inline (`aria-invalid` +
  `role="alert"`). Success behavior is unchanged: insert/update then
  redirect. The generator promotes its internal decode struct to a public,
  validating `{Model}Form` (derives `Serialize`/`validator::Validate`/
  `Default`, carries any `--validate` rules, and gets a generated
  `From<&Model>` to seed the edit form). The pre-existing `unique`-field
  duplicate-value re-render (#1032) now goes through the same changeset
  renderer instead of its own hand-rolled path. `examples/bookmarks`
  demonstrates the round-trip.

- **generate:** `decimal` scaffold field type (#1038) — `price:decimal`
  (default `NUMERIC(12,2)`) or `price:decimal{10,2}` for an explicit
  precision/scale now generates an exact-precision Postgres `NUMERIC` column,
  a `rust_decimal::Decimal` `#[model]` field, and a `Numeric`/`Nullable<Numeric>`
  Diesel schema token, so money-shaped fields no longer have to fall back to
  `f64` and its binary-float rounding. Both `decimal`/`Decimal` casings are
  accepted (consistent with `Attachment`/`attachment`), it composes with
  `Option<…>` and `:unique`, and the generated `new`/`edit` form renders
  through the changeset-aware `number_input` helper, sized to the column's
  scale. The `rust_decimal` dependency (with the `db-diesel2-postgres` and
  `serde` features) is added to the generated app's `Cargo.toml` only when a
  `decimal` field is present.
- **views:** `tabs` widget — a no-JavaScript panel switcher for detail and
  settings views (#1316). `tabs(id, panels)` takes an ordered
  `&[(id, label, maud::Markup)]` list and renders an `autumn-tabs` root with
  a `role="tablist"` strip of `role="tab"` anchors (each linking to
  `#panel-id`) and matching `role="tabpanel"` sections (`aria-labelledby`,
  `tabindex="0"`). Switching is pure CSS: `input.css` shows the panel whose
  `id` matches the URL's `:target` fragment, with a `:has()`-based fallback
  that shows the first panel when nothing is targeted (and an
  `@supports not selector(:has(a))` rule so browsers without `:has()`
  degrade to "first panel always shown" instead of a blank widget), so a
  3-tab detail/settings view needs zero hand-written JS or CSS and under 10
  lines of Maud. The active-tab highlight also tracks whichever panel is
  actually `:target`-ed, via position-based `:has()`/`:nth-child()` CSS
  (covering the first 6 tabs). Tab/panel ids and labels are HTML-escaped by
  Maud; panel bodies are pre-rendered `Markup` the caller owns, same as
  `card`'s `body` parameter. Panel ids must be unique across the whole page
  if more than one `tabs()` widget is rendered; `aria-selected` reflects
  only the server's default selection, since the server never sees the URL
  fragment. See `docs/guide/tabs.md` for a full example and these caveats.

- **views:** `modal`/`confirm_action` widgets — an accessible, testable
  confirm for destructive actions (#1233). `modal(id, title, body, config)`
  renders a native `<dialog>` (`aria-modal="true"`, `aria-labelledby`, a body
  slot, and an optional footer slot via `ModalConfig::footer`) with only
  `autumn-modal*` BEM classes. `confirm_action(id, trigger_label, action,
  method, csrf_token, config)` composes it with `link_to`/`button_to`'s
  `button_to_with` (#1138) into a full confirm flow: a trigger button opens
  the dialog, the cancel button closes it, and the confirm button is a real
  `button_to_with` submit carrying the correct HTTP method (`_method`
  override) and CSRF token, so a `TestClient` test can assert the dialog,
  its title, and the confirm button's action/method/CSRF token — impossible
  with the native `window.confirm()`/htmx `hx-confirm` it replaces. Opening
  and closing require zero app-authored JavaScript: triggers use the native
  `command`/`commandfor` HTML Invoker Commands API, with a `data-modal-open`/
  `data-modal-close` fallback shipped in `autumn-widgets.js` for browsers
  that don't support it yet. `showModal()` gives ESC-close, a focus trap,
  and focus-into/-return-from the dialog for free from the browser in both
  paths. The confirm button carries a danger semantic class
  (`autumn-modal__confirm--danger`) by default (`ConfirmActionConfig::
  danger(false)` to opt out), and `ConfirmActionConfig::light_dismiss`/
  `level`/`modal_class` pass through to the underlying `ModalConfig` for
  callers that need them. A `<noscript>` fallback renders the same confirm
  button directly (unconfirmed, matching a plain HTML form) so the action
  stays reachable with JavaScript disabled, since the trigger's `command`
  attribute alone can't open the dialog before JS runs. The admin plugin's
  detail-view delete button and bulk-action confirm are migrated to
  `confirm_action`/`modal`, dropping their `hx-confirm`/`window.confirm()`
  reliance entirely; the bulk-action dialog's Confirm button now closes via
  the same `data-modal-close` mechanism as its Cancel button, and its
  triggering form is re-queried rather than stashed on the dialog node.
  The bulk-action confirm keeps a `window.confirm()` fallback, reached only
  when `<dialog>.showModal` is unsupported, so a destructive action is
  never submitted without some confirmation. Purely additive; minor
  version bump.
- **mcp:** plugins and repositories can now layer in MCP. Chainable route
  toggles `Route::mcp()`, `Route::mcp_exclude()`, and `Route::mcp_stream()`
  mirror the `#[api_doc(mcp)]` / `mcp = false` / `mcp, stream` attribute
  forms at registration time, so a plugin can offer a fluent
  `MyPlugin::new().expose_mcp()` switch and let the *host* decide at install
  time whether the plugin's typed routes become MCP tools — no source
  attributes needed on handlers the host doesn't own (the flags are inert
  unless the host enables the `mcp` feature and calls `mount_mcp`). The
  `#[repository]` macro gains an `mcp` key:
  `#[repository(Model, api = "/path", mcp)]` exposes all five generated CRUD
  routes as tools and `mcp = "read"` exposes only list/get, with the usual
  verb-derived safety annotations (`readOnlyHint` on reads,
  `destructiveHint` on delete); `mcp` without `api = "/path"` is a compile
  error. Duplicate `mcp` keys are a compile error rather than silently
  last-write-wins. To support the generated `DELETE`, routes declaring an
  empty-body success status (`204 No Content`, `205 Reset Content`) are no
  longer conflated with schema-less HTML routes: they are MCP-eligible under
  an explicit opt-in, and an *untagged* read-only `204`/`205` route is now
  also auto-included under a pre-existing `expose_all_as_mcp()` hatch —
  hatch users can gain tools on upgrade. The result of a successful call to
  such a tool is enforced to be empty text, so a route that mislabels its
  status can't leak a response body to agents.
  `McpToolInfo` gains public read accessors (`name()`, `description()`,
  `input_schema()`, `annotations()`, `method()`, `path_template()`,
  `streams()`) so hosts can introspect derived tools. Routes registered by
  plugins via `routes()`/`scoped()` were already derived into tools; this is
  now pinned by tests and documented. Raw `nest()`/`merge()` routers remain
  MCP-invisible (documented follow-up). See the
  [MCP guide](docs/guide/mcp.md) §9–§10.

- **cli:** `unique` field-DSL marker and `--unique FIELD` flag for `autumn
  generate model`/`scaffold`/`migration` (#1032). `email:String:unique`
  scaffolds a `CREATE UNIQUE INDEX idx_<table>_<field>_unique` in the
  migration — a distinct name from the plain, non-unique `--index` output, so
  the two never collide even on the same column — restored on `RemoveXFromY`
  rollback (same precedent as `references`'/`enum`'s constraint restoration).
  `--unique FIELD` is the flag-based equivalent, mirroring `--index`'s
  ergonomics; both converge on the same `Field::unique` bit. A scaffolded
  unique field also gets a derived `find_by_<field>` repository lookup for
  free (no `--query` needed), and the generated HTML `create`/`update`
  handlers catch a duplicate submission — mapped once, in `autumn-web`, from
  Postgres SQLSTATE `23505` via the new `autumn_web::error::
  unique_violation_field` — and re-render the form with an inline
  "already exists" field error and `422`, instead of a generic `500`. The
  auth generator's hand-rolled `email … UNIQUE` column now goes through the
  same shared `unique_index_sql` primitive rather than a parallel raw-SQL
  path. `--dry-run` lists the migration file in the plan and `--help`
  documents the new token; single-column only (composite `UNIQUE(a, b)`,
  case-insensitive uniqueness, and the `--api` JSON conflict response are out
  of scope for this slice). The generated unique index name is disambiguated
  against Postgres's 63-byte identifier limit and against a coincidentally
  same-named plain index, including one added by an earlier, separate
  `generate` invocation (via `src/schema.rs`) — but not against a *plain*
  index whose own long name Postgres silently truncates to the same stored
  identifier, since plain index names carry no truncation handling of their
  own here (a broader, pre-existing gap, not specific to `unique`).

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
  hand-rolled delete button now uses `button_to_with` (honoring a
  customized `security.csrf.form_field` via `.csrf_field(...)`), gaining a
  working no-JS fallback (POST + `_method=DELETE` + CSRF) for free. `attrs`
  entries that collide with a named option (`href`/`class`/`target`/`rel`
  for `link_to`, `type`/`class` for `button_to`) panic rather than silently
  emitting duplicate-attribute markup, and `target="_blank"`/`rel="noopener"`
  handling is ASCII-case-insensitive. Purely additive; minor version bump.

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
- **cli:** `enum{a,b,c}` field type for `autumn generate model`/`scaffold`/
  `migration` (#1030). `status:enum{draft,published,archived}` scaffolds a
  closed-set column end to end: a generated `PascalCase` Rust enum (`Status`)
  wired for Diesel `TEXT` storage (manual `ToSql`/`FromSql` over
  `AsExpression`/`FromSqlRow`) and `serde`; a `CHECK (status IN (...))`
  constraint in the migration so an out-of-set `INSERT` fails at the database
  layer (restored on `RemoveXFromY` rollback, matching the `references`
  field's FK-restoration precedent); a `<select>` form widget matching the
  admin generator's `--select` output (auto-derived for `generate admin` too,
  unless an explicit `--select` overrides it); and request-boundary
  validation that rejects an out-of-set form value with a 400 naming the
  field rather than a 500 or silent coercion. `--default field=variant` sets
  both the SQL `DEFAULT` and the enum's `#[default]` variant; an unknown
  variant errors at generate time, as does a non-identifier or keyword
  variant (`enum{2fa}`). Nullable (`Option<enum{...}>`) is supported. The
  generated scaffold smoke test asserts an out-of-set value is rejected at
  both the database layer (raw `INSERT` fails, zero rows written) and the
  request boundary (400). Not supported by `generate job` (no model file to
  declare the type in) or `--query` (no import path for it yet). Quote the
  token in bash/zsh — an unquoted `enum{a,b}` is brace-expanded by the shell
  before `autumn` ever sees it; the parser detects this and suggests
  quoting.
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
- **ui:** Framework-owned widget stylesheet (issue #1215) — every semantic
  `autumn-*` class emitted by form fields, the submit button, active
  search/autocomplete, the nav bar, breadcrumbs, hero banners, modals, tabs,
  pagination, property lists, direct-to-storage upload, and job status is now
  backed by one shipped, token-themeable stylesheet (`autumn_web::ui::WIDGETS_CSS`,
  served at `autumn_web::ui::WIDGETS_CSS_PATH`) instead of a ~264-line block
  copy-pasted into every app's `input.css`. Plain CSS (no Tailwind build
  required), embeddable in single-binary release builds (#1004), and
  re-themed by overriding the existing `ui::tokens` design tokens rather than
  forking the component CSS. The `autumn new` template and every example's
  `input.css` no longer inline the widget CSS; a coverage test
  (`widget_css_coverage`) fails the build if a widget ever emits an `autumn-*`
  class with no backing rule. Additive API surface. **Visual change:** the
  previous per-app copy hardcoded an indigo accent (`#4f46e5`) independent of
  `ui::tokens`; the shipped stylesheet instead references `var(--primary)`,
  so widgets now pick up the framework's existing brand token (violet,
  `#7c3aed`) by default. Apps that want the old accent back can set
  `--primary: #4f46e5;` (and friends) on `:root` in their own stylesheet,
  loaded after the widget one. See [Widget styling](docs/guide/widget-styling.md).
  The framework CSS routes (`autumn_web::flash::FLASH_CSS_PATH`,
  `autumn_web::ui::WIDGETS_CSS_PATH`) also serve pre-compressed gzip/brotli
  bodies (computed once per process, not per request) when the client's
  `Accept-Encoding` accepts them, instead of relying solely on the
  general-purpose compression middleware to redo that work on every request.
- **cli:** `autumn test` provisions and targets an isolated test database before
  running your suite — it resolves the test DB URL with the same precedence as
  `autumn migrate` (`AUTUMN_DATABASE__PRIMARY_URL` → `AUTUMN_DATABASE__URL` →
  `DATABASE_URL` → `autumn.toml`), derives
  a `_test`-suffixed database name, creates it if missing, runs all pending app +
  framework migrations, exports `AUTUMN_ENV=test` and the resolved
  `DATABASE_URL`, then shells out to `cargo test` (exiting with its code).
  `--reset` drops and recreates the test DB first, and trailing `-- <args>` are
  forwarded to the harness (`autumn test -- --nocapture`). Refuses to run against
  a non-`_test` database (issue #1056).
- **test:** `TestResponse::query_count()` and
  `TestResponse::assert_max_queries(n)` turn the `Server-Timing` query counter
  into a test assertion, so you can pin a route's SQL budget and catch N+1
  regressions — chained after a request, `assert_max_queries` panics naming the
  route when the observed count exceeds `n` (issue #1262).
- **widgets:** server-rendered, accessible, zero-JavaScript SVG chart helpers in
  `autumn_web::widgets` (all prelude re-exported, with `/_stories` gallery
  entries) — `sparkline`, `bar_chart` (bars anchored at zero), and `line_chart`,
  each with a `_with` variant taking a `ChartConfig` builder (`.title(...)`,
  `.min(...)`/`.max(...)` axis override, accessible name) (issue #1231).
- **jobs:** opt-in versioned job payloads with an upgrade path — annotate a
  handler with `#[job(version = N, upgrade = ...)]` to wrap its args in an
  `{ "__autumn_schema_version": N, "args": … }` envelope, and the `upgrade` hook
  (`fn(u32, Value) -> Result<Value, E>`) migrates older stored payloads on the
  fly so rolling deploys drain the old queue instead of dead-lettering. Jobs with
  no version are stored raw (zero behaviour change); runtime helpers live in
  `autumn_web::payload_version` (issue #1205).
- **repository:** `#[repository(...)]` associations can now declare a
  `dependent(ChildRepository, fk = "col", on_delete = …)` cascade, so deleting a
  parent handles its children in one transaction —
  `on_delete` = `destroy` (soft-delete-aware, fires child hooks) | `delete_all` |
  `nullify` | `restrict` (probes for referencing rows before mutating and errors if
  any still exist) (issue #1369).
- **macros:** `dependent(...)` cascade delete is now recursive, bulk-aware, and
  declarable model-side. Deleting a parent that declares dependents cascades
  into grandchildren and deeper in the same transaction — each destroyed child
  runs its own `dependent(...)` cascade before its row is removed, with a
  `(table, id)` cycle guard so self- or mutually-referential graphs terminate
  instead of looping (`delete_all` stays single-level by design) (issue #1739).
  The generated `delete_many` bulk path runs the same
  destroy/`delete_all`/`nullify`/`restrict` cascade per affected parent inside
  its existing transaction — restrict probes first (a `409` rolls the whole
  batch back), then children, then the bulk parent delete — so bulk-deleting
  parents with dependent children no longer orphans or FK-errors those rows
  (issues #1740, #1787). Cascades can now be declared on the model with
  `#[has_many(Child, dependent = <action>)]` / `#[has_one(...)]` (equivalently
  `on_delete = <action>`) instead of the repository attribute: the model derive
  emits a runtime `Model::dependents()` that the repository codegen consults
  when no repository-side `dependent(...)` is present (the repository attribute
  still wins when both are declared), so `#[has_many(Child, dependent =
  destroy)]` drives the same transactional cascade — grandchild recursion and
  the `delete_many` bulk path included — with no repository attribute required.
  This model form ships for `destroy`, `delete_all`, and `restrict`; `dependent
  = nullify` and both-sites conflict diagnostics are deferred follow-ups, and a
  `dependent`/`on_delete` on a `through = <join_table>` association is a directed
  compile error rather than a mis-targeted cascade (issue #1738). See
  `docs/guide/repositories.md`.
- **audit:** version/audit writes are auto-attributed to the current actor. A new
  `autumn_web::current` module carries a request-scoped actor
  (`Current::set_actor` / `Current::actor`, plus `Current::set_default_actor` for
  jobs and the scheduler); `VersionEntry.actor` now records the authenticated
  user with no per-call plumbing, and falls back to `"system"` when unset — the
  `_autumn_version_history.actor` column is `NOT NULL DEFAULT 'system'` and the
  generated repository code substitutes `VersionEntry::SYSTEM_ACTOR` (issue #1383).
- **auth:** configurable password policy and persistent "remember me" login, both
  scaffolded automatically by `autumn generate auth`. `[auth.password]`
  (`min_length`, `reject_common` against a bundled weak-password corpus,
  `breach_check` = `off` | `fail_open` | `fail_closed` HIBP lookups) is enforced
  via `autumn_web::auth::PasswordConfig`/`PasswordPolicy`; `[auth.remember]`
  (`enabled`, `duration_secs`, `cookie_name`) issues selector/verifier remember
  cookies backed by a `{user}_remember_tokens` table (issues #1345, #1397).
- **api:** generated `#[repository(api = ...)]` JSON list endpoints now return a
  page envelope — `content`, `page`, `size`, `total_elements`, `total_pages`,
  `has_next`, `has_previous` — driven by `?page=`/`?size=` query params, and
  create/update handlers validate the decoded payload against the model's
  `#[validate(...)]` rules before hitting the database, returning **422 Problem
  Details** with a per-field `errors` map. Payloads without `Validate` compile to
  a no-op via the autoref `MaybeValidate` specialization (issues #1237, #1253).
- **cli:** first-class `autumn db backup` and `autumn db restore` with retention.
  `db backup [--dir DIR] [--format custom|plain] [--keep N] [--shard NAME]
  [--control-only]` dumps the control DB and every shard into
  `<dir>/<profile>/<timestamp>/` with a `manifest.json`, integrity-checks each
  artifact with `pg_restore --list` before reporting success, and `--keep N`
  prunes to the newest N runs. `db restore <ARTIFACT> [--shard NAME] [--force]`
  verifies every artifact before touching a database and is gated by the same
  production guard as `db drop` (issue #1595).
- **security:** multipart uploads are validated by magic bytes rather than the
  spoofable client `Content-Type` — the extractor sniffs the real content type
  whenever an `allowed_content_types` allow-list is configured or strict mode is
  on. Set `security.upload.reject_on_content_type_mismatch = true` to reject a
  declared-vs-sniffed mismatch (or an unsniffable declared-binary upload) as a
  spoof (issue #1354).
- **app:** web/worker process roles for independent scaling — a process `role`
  (`combined` | `web` | `worker`), set via `role = "…"` in config or the
  `AUTUMN_ROLE` env var, selects whether it serves HTTP, runs job workers + the
  cron scheduler, or both (`combined`, the default, is unchanged; `web` can still
  enqueue jobs). Run with `autumn serve --role web|worker|combined`, and
  `release init --split-workers` splices a dedicated `worker:` service into the
  generated docker-compose output. A split (non-combined) role requires a
  `postgres`/`redis` jobs backend, since an in-memory queue can't cross processes
  (issue #1613).
- **test:** fake-data factories for models plus bulk seed generation. A new
  `autumn_web::fake` module exposes deterministic fake generators (`name`,
  `email`, `sentence`, `int_range`, `uuid`, … seeded via `AUTUMN_FAKE_SEED` or
  `reseed`), and `#[model]` now generates a `{Model}Factory` with per-field
  setters, `.fake()`/`.fake_all()` to fill unset fields with realistic data
  inferred from field name + type, and `.build`/`.build_many`/`.create`/
  `.create_many`. `autumn seed --count N --model <Name>` (used together)
  generates and inserts N faked rows via the model's factory instead of running
  the hand-written seed body (issue #1343).
- **alerts:** operator alerts on critical production failures — email and
  signed-webhook notifications delivered through your existing mailer and
  outbound-webhook machinery with no application code. Configure a destination
  under `[alerts]` (`email` / `webhook_url` + `webhook_secret`) and every
  built-in condition fires, deduplicated with a recovery notice when it clears:
  a dead-lettered job, a health indicator down past `health_grace_secs`, the
  rolling 5xx rate crossing `error_rate_threshold`, or a framework-scheduled
  task failing. Each alert carries a stable `dedup_key`, a `critical`/`recovery`
  severity, the host/replica, and a "where to look" actuator pointer; webhooks
  are always signed (`webhook_secret` required). Custom transports plug in via
  `AlertChannel` + `AppBuilder::with_alert_channel`, and `autumn doctor` warns
  (in production) on a missing or unusable destination (issue #1610).
- **macros:** `#[validate(...)]` rules declared on an `UpdateModel` are now
  enforced on `PUT`/`PATCH` updates, not only on create, so a partial update
  rejects invalid input the same way a create does (issue #1719).
- **jobs:** per-queue reserved/concurrency worker pools — a `[jobs.queues]`
  value can be a table with `reserved = N` (dedicated slots no other queue may
  consume) and `concurrency = N` (a hard cap on a queue's share of
  `jobs.workers`), plus `jobs.pin` (or `AUTUMN_JOBS__PIN`) to pin a
  `worker`-role process to a subset of queues, and per-queue actuator gauges (a
  `queues` key with `depth` / `oldest_waiting_age_ms`) on
  `<actuator-prefix>/jobs` (issue #1623). The resolved `ProcessRole` is exposed
  on `AppState` via `state.role()` (`serves_http()` / `runs_workers()`), so
  app-owned background loops can self-gate to the right tier (issue #1726).
- **cli:** authenticated account-management flows scaffolded by
  `autumn generate auth` — change-password (`/account/password`) and
  change-email (`/account/email`), both `#[secured]` and behind a fresh
  `#[step_up]` claim, verifying the current password and re-rendering at **422**
  on bad input, with the current device kept signed in (issue #1396) — plus
  `--magic-link` passwordless email login, which adds a `magic_link_token`
  model + `magic_link_tokens` table and single-use, expiring login-link routes
  (issue #1328).
- **scaffold:** richer `autumn generate scaffold` output — a `belongs_to`
  foreign key (`post:references`) renders as a populated `<select>` dropdown
  labeled by the parent's display column (overridable with `{label:col}`), with
  index/show views showing the parent's display value instead of the raw `*_id`
  (issue #1146); a trailing `{…}` block on a field declares a constraint once
  and emits **both** a server-side `#[validate(...)]` rule and the matching
  HTML5 input attribute — `{min=N,max=N}`, `{email}`, `{url}` (issue #1388); and
  when a `src/bin/seed.rs` exists the generator idempotently links the
  `schema`/`models` modules into it so `autumn seed --count/--model` resolves
  the model's factory (issue #1718).
- **http:** SSRF hardening for the outbound HTTP client, all opt-in — the
  default path (shared client, reqwest auto-follow) is unchanged.
  `RequestBuilder::no_redirect()` returns a 3xx verbatim and
  `follow_redirects(max, validator)` follows a bounded number of hops, resolving
  each `Location` to an absolute URL and calling the validator before following
  (rejecting with `RedirectRejected` / `TooManyRedirects`) (issue #1238);
  `RequestBuilder::pin_to(addr)` pins the connection to a validated socket
  address (skipping DNS while preserving Host/SNI) to close DNS-rebinding/TOCTOU
  gaps (issue #1239). `Client::get_ssrf_safe(url)` composes the safe path —
  resolve once, validate every resolved IP against the built-in SSRF deny-list
  (public `is_blocked_ip` / `is_public_ip` helpers, IPv4-mapped/compat and
  NAT64/6to4-tunnelled forms unwrapped and re-checked), pin to a validated
  address, then follow redirects with per-hop resolve→validate→pin and an
  https→http downgrade guard. New `ClientError` variants `SsrfBlocked`,
  `TooManyRedirects`, `RedirectRejected`, `InvalidUrl` (issues #1238, #1239).
- **db:** offsite S3 backups — `autumn db backup --upload` (or
  `[backup.offsite] auto_upload = true`) now uploads each completed local run to
  an S3-compatible offsite destination (AWS S3, MinIO, Cloudflare R2, Backblaze
  B2, Garage) *after* local integrity verification passes, then HEAD/GET-verifies
  every remote object matches the local file before reporting success; a
  local-good / upload-failed run exits non-zero with an unambiguous split-outcome
  message and leaves the local artifact intact. Configure the destination in
  `autumn.toml` under `[backup.offsite]` / `[backup.offsite.s3]` (`bucket`,
  `region`, `endpoint`, `force_path_style`, `prefix`, `keep`,
  `allow_shared_bucket`, and credential *indirection* via `access_key_id_env` /
  `secret_access_key_env` — the secret values never live in config, argv, logs, or
  errors), with `AUTUMN_BACKUP__OFFSITE__*` env overrides (e.g.
  `AUTUMN_BACKUP__OFFSITE__S3__BUCKET`) and profile overlays
  (`[profile.prod.backup.offsite]`). Objects are keyed
  `{prefix}/{profile}/{timestamp}/{file}`; independent remote retention
  (`keep = N`) prunes older uploaded runs only after a verified upload (never the
  just-uploaded run). New `autumn db offsite list` shows the offsite runs for the
  active profile, and `autumn db restore offsite:<profile>/<timestamp|latest>`
  (or `--offsite`) downloads a run to a temp dir and applies the same integrity
  verification and production `--force` guard as a local restore. Transfers use a
  dependency-light synchronous SigV4 S3 client streamed end-to-end to bound memory
  (multipart above 64 MiB — S3 caps a single `PutObject` at 5 GiB — sending a
  server-side `x-amz-checksum-sha256`); pointing the offsite bucket at the app's
  own `[storage.s3]` bucket requires the explicit `allow_shared_bucket = true`
  opt-in (issue #1619).
- **doctor:** new `offsite_backup` check — informational (never a hard failure,
  never prints a credential value). It notes when no `[backup.offsite]`
  destination is configured, and once one is, flags an unset
  `backup.offsite.s3.bucket`, a destination that shares the app's `[storage.s3]`
  bucket without `backup.offsite.allow_shared_bucket = true`, or named credential
  env vars that are not ready (issue #1619).
- **alerts:** a failed `autumn db backup` offsite upload now raises a
  `ScheduledTaskFailure` operator alert (dedup key
  `scheduled_task_failure:db-backup-offsite-upload`, title "Offsite backup upload
  failed") through the configured `[alerts]` channels (PagerDuty / Slack / Discord
  + signed webhook), so an unattended/cron backup never fails its upload silently.
  Delivery is best-effort on a short-lived runtime and can never change the
  command's exit code; with no `[alerts]` destination configured no channels are
  built, so the interactive case (message + non-zero exit) is unchanged
  (issue #1743).
- **ci:** the offsite-backup disaster-recovery round-trip
  (`autumn-cli/tests/integration/offsite_backup.rs`) now runs in GitHub Actions —
  `cargo test -p autumn-cli --test cli_tests -- --ignored offsite` drives
  seed → `autumn db backup --upload` → restore-from-offsite → row-level equality
  against real MinIO + Postgres testcontainers. The Docker-dependent Linux step
  installs `postgresql-client` (so the host `pg_dump`/`pg_restore` that `autumn db
  backup` shells out to are on PATH) and gains `timeout-minutes: 30`; the
  round-trip stays in the existing Docker-dependent step rather than a new
  required gate (issue #1744).
- **scaffold:** a scaffolded required numeric field carrying a `{min,max}` range
  constraint (issue #1388) is now emitted as `Option<T>` on the generated
  `*Form` struct with `#[validate(required, range(...))]` instead of the native
  `i32`/`i64`/`f32`/`f64`. A native numeric defaults to `0`, which pre-fills the
  input, so a blank submission used to pass both the HTML `required` attribute
  and the server-side `range` rule whenever the declared range spans the type's
  zero default (e.g. `age:i32{min=0,max=130}`), silently coercing the field to
  `0`. `Option<T>::default()` is `None`, so the input renders blank; `required`
  rejects `None` with a **422** inline error and `range` still validates the
  inner value when `Some`. `into_new` unwraps the validated `Some(_)` (a missing
  value is a bad request naming the field), `from_row` wraps a persisted native
  value in `Some`, and the empty `field=` pair is dropped by the form decoder so
  a blank non-browser submit surfaces the 422 rather than a `400` from parsing
  `""`. The `required` rule lands only on the form struct, never on the
  native-typed model field (issue #1748).
- **scaffold:** `--live-validation` scaffolds now emit the client-side HTML5
  constraint attributes (`minlength`/`maxlength`, `type="email"`/`type="url"`,
  numeric `min`/`max`, `step="any"`, and a `<textarea>` for a constrained `Text`
  column) and the `belongs_to` parent `<select>`, both of which the per-field
  live path previously dropped — the #1388 client-side hints were only emitted on
  the standard `form_for` path, and `reference_fields` was zeroed out under
  `--live-validation`, so a `references` field leaked out as a plain text input.
  The live path now renders DSL-constrained `String`/`Text`/numeric fields and
  the reference dropdown as raw maud markup carrying the same HTML5 attributes and
  ARIA/inline-error skeleton the standard path produces; the reference option
  loaders are threaded into the new/edit form bodies (and the 422 re-render
  branches) so the `<select>` is populated at request time with changeset-driven
  `selected` state, and the inline-validate handlers return the identical
  constrained fragment so an htmx `outerHTML` swap on `change` no longer sheds the
  client-side guards. Server-side `#[validate(...)]` was already applied under
  `--live-validation`; this restores only the client-side HTML5 hints and the
  parent dropdown (issue #1750).
- **actuator:** backend-derived `/actuator/jobs` queue gauges on the durable
  backends. On Postgres/Redis the per-queue `depth` / `oldest_waiting_age_ms`
  and the per-job-type `queued` counters are now surveyed from the durable store
  and wholesale-replace this process's local enqueue marks each tick, so an
  enqueue-only `web` replica reports the true shared backlog instead of its own
  ever-growing local marks; a queue absent from the latest survey resets to
  `depth` 0, so stale backlog never leaks between ticks. The Redis survey runs
  every 2s (the interval doubles as the gauge cache TTL) and pages the entire
  due-delayed ZSET so scheduled/retry bursts are counted exactly, emitting a
  single `warn!` if a pathological backlog is truncated at the scan cap. The
  `local` backend keeps its in-process mark path unchanged (issue #1752).
- **doctor:** topology-aware `autumn doctor --strict` queue-coverage
  (`jobs_queue_coverage`). Declare the fleet's worker tiers under
  `[jobs.fleet] tiers = [["critical"], ["bulk", "default"]]` (each inner array is
  one `worker` tier's `jobs.pin`; an empty array is an unpinned tier that drains
  every queue) and doctor proves coverage topology-wide, hard-failing only when a
  needed queue — the configured `[jobs.queues]` unioned with the
  `#[job(queue = "…")]`-declared set — is drained by no tier anywhere, so a valid
  multi-tier subset split no longer false-positives. The job-declared set is
  resolved from `[jobs.fleet] manifest = "<path>"` (a `queues = [...]` manifest
  the app emits) or an inline `[jobs.fleet] declared_queues = ["…"]`. Absent
  `[jobs.fleet]` the check stays informational-only, exactly as before, so no
  existing deployment regresses (issue #1756).
- **cli:** `autumn jobs manifest <path>` emits the running app's effective
  drained-queue manifest. It compiles the app (debug profile) and runs it under
  `AUTUMN_DUMP_JOBS=1` to capture the ground-truth drained-queue set — the
  configured `[jobs.queues]` unioned with every `#[job(queue = "…")]`-declared
  queue, including synthesized durable-listener queues — without binding a port
  or touching a database, then writes a TOML `queues = [...]` document to
  `<path>`. `autumn doctor` consumes it via `[jobs.fleet] manifest = "<path>"`,
  so the topology coverage check sees exactly what the runtime drains rather than
  a hand-maintained list. `--package` / `--bin` select the target in a
  workspace, and the captured stdout is validated as a TOML `queues` string
  array before it is written (issue #1756).
- **tenancy:** per-tenant in-process memory cells — each resolved tenant gets a
  `TenantCell`, a byte-accounting boundary with a soft memory quota and an owned
  scratch buffer, minted lazily by the process-wide `TenantCellRegistry` on the
  first call to `current_tenant_cell()`, so routes that never touch tenant
  memory allocate no cell. `try_charge(n)` reserves bytes against the quota and
  returns a `Charge` RAII guard that releases them on drop; the per-tenant
  scratch store (`scratch_insert`/`scratch_get`/`scratch_remove`) is charged by
  allocation capacity plus a fixed per-entry overhead. A charge that would
  exceed the quota fails only the offending tenant's request with `QuotaExceeded`
  → HTTP **503 Service Unavailable**, leaving every other tenant's independent
  counter untouched; `TenantCellRegistry::evict` deterministically reclaims a
  cell's tracked footprint on `Drop` while an in-flight request keeps its cached
  cell to completion. Configure under `[tenancy]` with `quota_bytes` (`0`, the
  default, disables the quota). Follow-ups add bounded (`max_cells`, LRU) and
  idle (`idle_ttl_secs`) registry eviction, enforced lazily on cell insert, plus
  a reusable `evict_idle_older_than` primitive (issue #1792); store the soft
  quota atomically and refresh a resident cell from the configured value on every
  access via `set_quota_bytes`, so a future config-reload path can retune it
  without rebuilding cells (issue #1783); and make the entire `[tenancy]` section
  settable from the environment through `AUTUMN_TENANCY__*` — including
  `AUTUMN_TENANCY__QUOTA_BYTES`, `AUTUMN_TENANCY__MAX_CELLS`,
  `AUTUMN_TENANCY__IDLE_TTL_SECS`, and `AUTUMN_TENANCY__JWT_SECRET` (issue #1793)
  (issues #1766, #1792, #1783, #1793).
- **alerts:** native `PagerDuty` / Slack / Discord alert transports, built on
  the `AlertChannel` fan-out seam from #1610 with no change to the core alert
  pipeline. Configure any of them under `[alerts]`: `pagerduty_routing_key`
  (Events API v2 integration key; optional `pagerduty_url` to target a
  PagerDuty-Events-compatible endpoint), `slack_webhook_url`, and
  `discord_webhook_url` (delivered via Discord's Slack-compatible endpoint —
  append `/slack`), each with an `AUTUMN_ALERTS__*` env override. PagerDuty
  events correlate on the alert's stable `dedup_key`, so a repeating condition
  folds into one incident and an autumn-side recovery auto-resolves it.
  Per-channel severity routing via `pagerduty_severities` / `slack_severities` /
  `discord_severities` (`"all"`, the default, or `"critical"` to page on failure
  but stay quiet on recovery); an alert below a channel's threshold is never
  delivered to it. All outbound calls reuse the SSRF-hardened HTTP client and
  Slack/Discord webhooks require absolute `https`. `autumn alert test
  [--channel <name>]` fires a synthetic alert through each configured
  outbound-HTTP channel and reports per-channel success/error, and `autumn
  doctor` gained an `alert_transports` check that (in production) flags a
  whitespace-mangled routing key, a non-absolute `pagerduty_url`, or a
  non-absolute-`https` Slack/Discord URL (issue #1630).

### Fixed

- **idempotency:** the in-memory idempotency store no longer panics on extreme
  TTL values. `Instant::now() + ttl` panics when the sum is not representable by
  the platform clock, so a pathological configured or attacker-influenced TTL
  (e.g. `Duration::MAX` / `Duration::from_secs(u64::MAX)`) could crash the
  process; deadlines are now computed with a saturating helper that clamps to a
  far-but-representable future.
- **observability:** `Server-Timing` no longer miscounts pooled-connection reuse
  in the `db` metric. Connections are pooled and diesel-async's deadpool manager
  never resets a connection's instrumentation on recycle, so a connection that
  served a prior measured request kept its stale `RequestQueryTimer` installed.
  On the next checkout the housekeeping `SET statement_timeout` ran while that
  stale timer was active — adding a bogus `+1 query` (and its latency) to the
  next request before any application SQL — and a reused connection kept paying
  per-query `DebugQuery` formatting even on later opted-out requests. `Db::checkout`
  now installs a fresh timer on every measured checkout (before the `SET`), the
  timer's `on_start` probes the request scope before formatting or recording
  anything (a cheap no-op for any stale timer left on a reused connection), and
  the checkout `SET statement_timeout` is classified as an uncounted
  housekeeping statement alongside transaction-control SQL (issue #1348).
- **observability:** `Server-Timing` no longer clobbers an application's own
  Diesel instrumentation. `Db::checkout` installs its `RequestQueryTimer` via
  diesel-async's `set_instrumentation`, which *wholesale replaces* a
  connection's instrumentation — so an unconditional install would overwrite
  any global hook an app registered with
  `diesel::connection::set_default_instrumentation` (query logging, tracing,
  metrics) on the first checkout and never restore it, silently disabling it
  even when `[observability] server_timing` is off. `Db::checkout` now installs
  the timer only when a `Server-Timing` request scope is active
  (`request_db_timing_active`), so an opted-out app keeps its own
  instrumentation intact. Composing autumn's timer with an app-provided default
  instrumentation is a documented limitation while `server_timing` is enabled —
  see `docs/guide/observability/server-timing.md` (issue #1348).
- **observability:** `Server-Timing` no longer installs the per-request DB
  query timer on checked-out connections when `[observability] server_timing`
  is disabled (the production default). Previously every checked-out connection
  received the instrumentation unconditionally, so each query paid a full
  `DebugQuery` SQL formatting/allocation on its `StartQuery` event before the
  no-op accumulator write discovered no request scope was active. `Db::checkout`
  now probes the request-scoped task-local (`request_db_timing_active`) and
  installs the timer only when the `Server-Timing` layer has scoped the request,
  so opted-out requests carry zero per-query instrumentation overhead. When
  enabled, behaviour (`db;dur`/query count) is unchanged (issue #1348).
- **generator:** the generated `README.md` now orders the `libpq` prerequisite
  before the `cargo install diesel_cli --features postgres` command, since that
  command's `postgres` feature (and the base `cargo build`, which links the `db`
  feature) needs the PostgreSQL client library. The DB-free `--daemon` README no
  longer advertises `autumn generate scaffold` or the `migrations/` layout row:
  that generator emits Diesel models/repositories/migrations requiring the `db`
  feature the daemon scaffold disables, so following it would leave the app
  non-compiling. `--bundled-pg` keeps the `db` feature and retains
  `generate scaffold` (issue #1052).
- **generator:** the `--daemon` and `--bundled-pg` READMEs now document
  `autumn dev` as the browser-reachable local run (it binds TCP on
  `127.0.0.1:3000`), matching the default README. The background daemon start
  (`autumn serve --daemon` / `autumn serve --bundled-pg`) is reframed as the
  production mode that binds a private Unix domain socket — not reachable at
  `http://localhost:3000` — with a pointer to `autumn serve status` for the
  socket address and `docs/guide/daemon.md` for details, so following the README
  no longer leaves the route unreachable in a browser (issue #1052).
- **views:** vendored the full Idiomorph 0.3.0 morphing script (replacing a
  minimal stub) so live `hx-ext="morph"` updates actually DOM-morph, and
  patched its htmx extension so non-morph out-of-band swaps (e.g.
  `beforeend`/`delete`) no longer throw a caught console error. Served the
  idiomorph script with a revalidating cache policy (a weak content-derived
  `ETag` plus `Cache-Control: public, max-age=0, must-revalidate`) instead of a
  year-long `immutable` cache, so clients that had cached the old stub pick up
  the real script on their next revalidation rather than running stale code for
  up to a year. (The idiomorph URL is not content-fingerprinted; adding
  fingerprinted asset URLs so it can safely go back to `immutable` caching is a
  possible future follow-up.)
- **actuator:** `PUT /actuator/loggers/{name}` no longer reports a false
  `{"status":"ok","applied":true}` for a change that did not actually reach the
  live subscriber (issue #1044). Logger names are now validated up front like
  levels — a name carrying an `EnvFilter` metacharacter (`=`, `,`, whitespace,
  …) is rejected with `400` instead of being stored as a bogus override — and
  the response is driven by the real apply outcome: if the directive fails to
  apply the override is rolled back so `GET /actuator/loggers` never advertises
  a level that isn't live. The directive is now applied while the state lock is
  held so concurrent updates apply in the same order they mutate the map,
  keeping `GET /actuator/loggers` consistent with live emission under
  concurrency (AC4).
- **cli:** the generated `build.rs` git-provenance rerun triggers are more
  reliable (issue #1242): it now also watches `.git/logs/HEAD` (which moves on
  every commit, `--amend`, and `reset --soft`, so the baked SHA/`dirty` flag no
  longer goes stale after an amend), resolves the real gitdir when `.git` is a
  *file* (git worktrees / submodules) instead of assuming the `.git/` directory
  layout, and honors `SOURCE_DATE_EPOCH` for a deterministic build timestamp on
  reproducible builds. Non-git builds still degrade gracefully (fields `null`,
  build never fails).
- **macros:** `has_many(dependent = …)` now emits a directed compile error for
  unsupported values instead of a confusing generic parse failure (issue #1702);
  `across_tenants()` rejects a query with no shard set instead of silently
  running it unsharded (issue #1692).
- **repository:** a sharded, tenant-scoped repository built without a shard set
  (e.g. via `with_pool_untracked`) now rejects `across_tenants()` reads across
  the whole read family instead of silently returning a partial single-pool
  result. `find_all`, `find_by_id`, `exists_by_id`, the derived `find_by_*`,
  `with_deleted`, and `only_deleted` previously fanned out only when a shard set
  was present and otherwise bound a `NULL` tenant predicate against just the
  current pool, returning an incomplete result; each now returns a "requires a
  configured shard set" `bad_request`, matching the `count`/batch guard from
  #1692 (covers both the owned- and borrowed-param derived-read branches)
  (issue #1741).
- **scaffold:** scaffolded views now render through the shared application
  layout instead of a bare standalone page, and flash rendering was migrated to
  the shared flash helper (issues #1130, #1240).
- **config:** `[backup.offsite]` is no longer materialized from optional-only
  environment keys. A bare `AUTUMN_BACKUP__OFFSITE__S3__REGION` (or `ENDPOINT`,
  `FORCE_PATH_STYLE`, `PREFIX`, `KEEP`) with no bucket or credential-env names
  used to create an otherwise-empty section that then failed validation /
  `autumn doctor --strict` with "backup.offsite.s3.bucket is unset". The
  materializing trigger set is now limited to the keys a working upload genuinely
  requires — `BUCKET`, `ACCESS_KEY_ID_ENV`, `SECRET_ACCESS_KEY_ENV` (plus a truthy
  `AUTO_UPLOAD`) — while the optional keys are still applied once the section is
  materialized by a required key (issue #1791).
- **db:** offsite upload now deletes a just-written remote object when its
  post-upload verification fails. Because `put_file_and_verify` writes the object
  *before* its HEAD/GET verification, a verify failure (or a transient read during
  verify) could leave a corrupt/partial object in the bucket; both the single-
  `PutObject` and multipart paths now route their final verify through a shared
  `verify_or_delete` helper that best-effort `DeleteObject`s the key on any verify
  error before returning the original (loud, non-zero) verify error — a failed
  delete is logged but never masks it (issue #1760).
- **security:** the generated magic-link verify handler (`POST
  /login/magic/verify`) now re-checks the account lock before establishing a
  session. A magic link minted before an account was locked previously bypassed
  the lockout policy — the handler consumed the single-use token and logged the
  user in without re-checking `locked_at`. It now runs a fresh guarded `UPDATE`
  on `locked_at` (the same time-bounded `[auth.lockout]` cool-off semantics as
  password login, gated on `lockout_enabled`, a no-op when lockout is disabled)
  that re-reads the current lock state at the DB — closing the concurrent-lock
  TOCTOU against the earlier in-memory row — and rejects when the account is
  actively locked. The recheck sits before the TOTP branch, covering both
  `--magic-link` and `--magic-link --totp`, and a locked account renders the
  same generic failure page as an expired/consumed/unknown token, so there is
  no oracle distinguishing "locked" from "bad link" (issue #1777).
- **macros:** `#[autumn_web::model]` / `#[repository]` now derive table
  names with rule-based English inflection instead of naively appending
  `s`, so `Category` maps to `categories` (matching the CLI scaffold's
  `pluralize_word`) rather than a nonexistent `categorys` schema module
  that failed to compile (E0433). Only the last snake_case segment is
  pluralized (`blog_post` → `blog_posts`), irregulars
  (`person` → `people`), sibilant endings (`+es`) and consonant-`y`
  (`+ies`) are handled, and the `has_many` default accessor plus the
  `add_`/`remove_` m2m helpers use the same rule — the latter derived from
  the target type's singular so `categories` still yields `add_category`,
  not `add_categorie`. The `#[model(table = "...")]` escape hatch is
  unchanged (issue #1753).
- **security:** the generated auth scaffold no longer grants step-up
  "sudo mode" when a session is restored from a remember-me cookie. A
  long-lived persistent cookie is not strong authentication, so
  `establish_remember_login` no longer stamps `set_last_strong_auth_at`,
  and because `rotate_id()` preserves session data it now actively clears
  any stale elevation carried over from a prior authenticated state in the
  same browser — both `last_strong_auth_at` (`STEP_UP_SESSION_KEY`) and the
  reauth flow's `reauth_pw_ok` marker. This forces `#[step_up]` routes
  (e.g. account deletion) to redirect to `/reauth` for a fresh password
  check, closing a path where a stolen or unattended remember cookie could
  silently pass elevated routes (issue #1397).
- **openapi:** `extract_path_params` no longer emits phantom or
  brace-carrying parameter names for escaped, regex-constrained, or
  unbalanced-brace route patterns during spec generation. Escaped literal
  braces (`{{hello}}`) now yield no parameter, `:constraint` suffixes are
  stripped to the bare name (`{id:[0-9]{1,3}}` → `id`), and a brace-free
  guard drops malformed candidates (`{a{b}` → none), keeping the emitted
  list brace-free. This path is `openapi`-only; routing is unaffected since
  matchit rejects such patterns at registration (issue #1721).
- **http:** htmx positional and `innerHTML` out-of-band swaps now render a
  `<div>` carrier instead of `<template>`, so `HtmxFragments` fragments
  sent via `oob_with_strategy` with `OobSwap::{BeforeBegin, AfterBegin,
  BeforeEnd, AfterEnd, InnerHTML}` actually land in the DOM. htmx applies
  these swaps by iterating the carrier's `childNodes`, which are empty for
  a `<template>` (its children live in `.content`); element-replacing swaps
  (`true` / `outerHTML`) keep the `<template>` carrier htmx unwraps by id.
  A fragment passed to a child-node-inserting swap must be a valid direct
  child of a `<div>` — a bare `<tr>`/`<td>`/`<tbody>`/`<option>`/`<col>` is
  foster-parented out and vanishes; use the element-replacing path with the
  id on the row for table-row swaps (issue #1688).

### Changed

- **generator:** scaffolded `create`/`update`/`destroy` handlers now redirect
  with `autumn_web::Redirect::to(...)` — a real **303 See Other** with a
  `Location` header — instead of the hand-rolled 200 meta-refresh HTML page,
  matching the `Redirect` primitive `docs/guide/path-helpers.md` already
  recommends (and which the new #1127 write-path test asserts). `create` and
  `destroy` redirect to the index route; `update` redirects to the show route.
- **security:** `TenancyConfig::jwt_secret` is now stored as a
  `secrecy::SecretString` instead of a plain `String`, so the JWT signing
  secret is redacted from `Debug` output (and any logs that format the
  config) and zeroized on drop. Config-file deserialization is unchanged —
  a plain TOML string still works. Breaking for code that read or set the
  field directly: set it with `Some(value.into())` and read it via
  `secrecy::ExposeSecret::expose_secret()` (supersedes #1304). [no-plugin]
- **deps(security):** dependency-vulnerability upgrades (supersedes PR #1557;
  `diesel-async` was already handled separately). `aws-sdk-s3` floored at
  1.122 (1.119.0 → 1.122.0, the last MSRV-1.88 release) with
  `default-features = false` to drop the deprecated legacy hyper 0.14 /
  rustls 0.21 connector stack — this removes `rustls-webpki` 0.101.7
  (RUSTSEC-2026-0104 / GHSA-82j2-j2ch-gfr8 high, RUSTSEC-2026-0098 /
  GHSA-965h-392x-2mh5 and RUSTSEC-2026-0099 / GHSA-xgp8-3hg3-c2mh low),
  `rustls` 0.21.12, `hyper` 0.14.32, and `h2` 0.3.27 from `Cargo.lock`
  entirely (the modern hyper 1.x / rustls 0.23 `default-https-client` stack,
  which is what the SDK actually uses at runtime, stays enabled); transitive
  `lru` 0.12.5 → 0.16.4 (RUSTSEC-2026-0002 / GHSA-rhfx-m35p-ff5j);
  `opentelemetry_sdk` 0.31.0 → 0.32.1 (CVE-2026-48504 / GHSA-w9wp-h8wv-79jx,
  unbounded memory allocation in W3C Baggage propagation) together with the
  matching `opentelemetry` / `opentelemetry-otlp` 0.32.0 and
  `tracing-opentelemetry` 0.33.0 (with the `tls-aws-lc`, `tls-roots`, and
  `reqwest-rustls` features enabled on `opentelemetry-otlp`, since 0.32
  rejects `https://` gRPC collector endpoints at exporter build time unless
  a TLS provider feature is on, the provider alone ships an empty trust-root
  store — `tls-roots` loads the platform's native roots, which the gRPC
  exporter now enables explicitly via `with_enabled_roots()` for `https`
  endpoints — and its new reqwest 0.13 HTTP client ships without TLS under
  `reqwest-client` alone — reqwest 0.12 feature unification no longer
  covers it).
  `rsa` 0.9.10 (RUSTSEC-2023-0071, Marvin
  attack) remains: no fixed release exists on its 0.9.x line. [no-plugin]
- **deps:** bumped `diesel-async` from 0.8 to 0.9 (resolving 0.9.2, with
  `diesel` at 2.3.10) and `libsqlite3-sys` from 0.36 to 0.37 — 0.37 is the
  newest release line diesel 2.3 accepts (`<0.38`). diesel-async 0.9 changed
  `AsyncConnection::transaction` to take `AsyncFnOnce` closures instead of
  `ScopedBoxFuture` callbacks; internal call sites and macro-generated code
  were ported via a new semver-exempt `scoped_transaction` adapter, so the
  public `Db::tx` / `Db::tx_with` / `savepoint` closure API (including
  `.scope_boxed()` usage in app code) is unchanged. The CLI generators now
  pin `diesel-async = "0.9"` in generated/starter `Cargo.toml`s and the
  auth generator emits `async move |conn|` transaction closures instead of
  `ScopedBoxFuture`-style `Box::pin` callbacks. [no-plugin]
- **workspace:** `Cargo.lock` is now committed to the repository (it was
  previously gitignored) so builds are reproducible and dependency updates
  are reviewable.
- **dev:** `autumn dev` now renders a full-screen browser overlay carrying the
  compiler diagnostics when a rebuild fails, instead of leaving a blank or stale
  page — injected under the strict nonce-based CSP and cleared on the next
  successful rebuild (issue #1115).
- **macros:** partial updates now validate the effective **merged model** (the
  existing row ∪ the patch, after normalization) on the `from_patch` update path,
  not only the patch struct's own fields. `#[model]` derives `validator::Validate`
  on the read model (gated on `has_validation`, symmetric with the `New*`/
  `Update*` models) and keeps the full field `#[validate(...)]` set there, and the
  generated `from_patch` validates the reconstructed concrete model before
  returning the draft — before `before_update`, mirroring create (validate before
  `before_create`). Because the merged model's fields are concrete `T` rather than
  `Patch<T>`, validators that cannot be enforced on `Patch<T>` — `ip` on `Option`
  fields and `does_not_contain` (E0119 trait-coherence walls under validator
  `0.20`) and the cross-field `custom`/`must_match`/`nested` (no single-field
  `Patch<T>` trait) — are now enforced on update too, returning the same **422**
  field-error map as create. This covers every update path that builds a draft via
  `from_patch` (repositories with `hooks = ...` and their `--api` handlers); the
  blind `__to_changeset` paths (plain/`api`/`policy` repositories without hooks)
  still run only the patch-struct validators (follow-up: issue #1801). The
  `Patch<T>` per-field impls and the `UpdateModel` denylist are unchanged, so the
  change is backward compatible (issue #1778).
- **cli:** the magic-link login flow scaffolded by `autumn generate auth
  --magic-link` now sources its token lifetime and per-email cooldown from
  `autumn.toml` instead of hard-coded `const`s, so operators can tune them
  without editing the generated handler. New `[auth.magic_link]` section on
  `AuthConfig`: `ttl_minutes` (default `15`; keep it ≤ 15 to bound a leaked
  link's blast radius) and `email_cooldown_secs` (default `60`; the per-email
  re-mint throttle). Both are unsigned, so a negative value in `autumn.toml`
  now fails deserialization rather than silently minting expired tokens or
  defeating the email-bomb throttle (issue #1737).

### Fixed

- **security:** CSRF and CAPTCHA exempt-path matching now normalizes the
  request path (resolving `.`/`..` dot-segments — including percent-encoded
  `%2e` forms — treating percent-encoded slashes (`%2f`) as segment
  separators, and collapsing duplicate slashes) before comparing it against
  exemption prefixes, so a request like `POST /api/../submit` or
  `POST /api/%2e%2e%2fsubmit` can no longer satisfy an `/api/` exemption
  while targeting a protected route through a downstream component that
  percent-decodes or resolves dot-segments (supersedes #1229).
- **mail:** the SMTP password can no longer leak into startup error messages.
  When `mail.smtp.password_env` names an environment variable whose contents
  are not valid unicode, the raw `std::env::VarError::NotUnicode` value (which
  carries the password itself) was formatted into the
  `MailError::InvalidMessage` text; the error now reports a static reason
  ("environment variable is not set" / "contains non-unicode data") alongside
  the variable *name* only, never its value. Supersedes PR #887.
- **circuit_breaker:** the breaker and its registry now recover from a
  poisoned mutex (`lock().unwrap_or_else(PoisonError::into_inner)`) instead
  of panicking on every subsequent call once a single lock holder has
  panicked; breaker state is a self-correcting sliding window, so the
  recovered data is safe to keep using. Supersedes #1207. [no-plugin]
- **docs:** repaired the broken intra-doc links in the `reporting` module
  overview (`ErrorEvent`, `ErrorReporter`, `LogReporter`, `ReportingLayer`
  rendered as dead links on docs.rs because shorthand references don't resolve
  when a module carries both outer and inner doc comments — they now use
  explicit `crate::reporting::…` paths) and dropped a redundant explicit link
  target in the `job_tracking` module docs. Supersedes the salvageable parts
  of PR #1555.
- **jobs:** fixed a first-initialization race in the process-global job
  client (supersedes #1491): `init_global_job_client` /
  `clear_global_job_client` used a get-then-set pattern on the backing
  `OnceLock`, so two threads racing the very first install/clear could have
  one side's `OnceLock::set` lose and be silently dropped — leaving a job
  runtime that had just installed its client invisible to `global_job_client()`
  (free-function `enqueue` and `#[job]` handlers would see no runtime). Both
  functions now use `OnceLock::get_or_init` so the slot is created exactly
  once and every install/clear lands through the `RwLock`. Both functions now
  also recover from a poisoned lock (`PoisonError::into_inner`) instead of
  silently skipping the write, and a loom model-check
  (`chaos_job_client_loom`) exercises the first-init race across all
  interleavings.

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

- **ci:** plugin freshness gate (`scripts/check-plugin-freshness.sh` +
  `.github/workflows/plugin-freshness.yml`) — a PR that adds entries to this
  changelog's Unreleased `Added`/`Changed` sections without touching the
  Claude plugin (`skills/`, `agents/`, `.claude-plugin/`) fails a fast,
  toolchain-free check; exempt individual bullets with a bracketed
  `no-plugin` marker, or the whole PR via the same marker in the PR body or
  the `plugin-exempt` label. The same job sanity-checks that
  `plugin.json` parses and that every `docs/guide/*.md` path referenced from
  the plugin exists. Run `scripts/check-plugin-freshness.sh --self-test`
  locally.

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

- **generate:** `autumn generate tauri-mobile` — Tauri v2 **mobile** scaffold
  (iOS/Android) that runs the Autumn Axum server **in-process** on a
  background thread against a remote Postgres database (issue #1507,
  Option B). Mobile sandboxes forbid sidecar processes, so the shell crate
  builds as staticlib/cdylib, links the app crate directly, spawns the server
  from `tauri::Builder::default().setup(...)`, health-polls `/health`, and
  opens the webview at `http://127.0.0.1:<port>`; a small pool
  (`AUTUMN_DATABASE__POOL_SIZE=2`) is pinned for flaky mobile networks (and
  `AUTUMN_DATABASE__CONNECT_TIMEOUT_SECS=5`, matching the framework default,
  is pinned explicitly). The generator also extracts the stock
  `src/main.rs` into `src/lib.rs::serve()` (anchored; skipped with a warning
  when customised). Docs: mobile sandboxing restrictions, flaky-network pool
  behavior, the loopback security model, and App Store / Google Play
  guideline compliance in
  [docs/guide/tauri-mobile-in-process.md](docs/guide/tauri-mobile-in-process.md).
  [no-plugin]

- **db:** TLS to Postgres. The connection pool now honors `sslmode` in the
  database URL via a rustls-backed connector: `sslmode=require` encrypts the
  connection (libpq parity — no certificate-identity check, so self-signed
  and private-CA servers work), and `sslmode=verify-full` additionally
  verifies the certificate chain and hostname against the Mozilla root store
  (plus an `sslrootcert=<PEM file>` when given). URLs without `sslmode` (or
  with `disable`/`prefer`) keep the previous plaintext behavior unchanged.
  Previously the pool hardcoded `NoTls`, so `sslmode=require` failed every
  connection with "no TLS implementation configured" and cleartext was the
  only working configuration (issue #1507). [no-plugin]

- **offline-sync (new feature):** offline-first local storage plus a sync
  engine for occasionally-connected apps such as the in-process Tauri mobile
  shell (issue #1508). `autumn_web::sync` ships a local SQLite `SyncStore`
  (JSON rows per collection, write-through change journal in the same
  transaction, tombstoned deletes), a client `SyncEngine` (push pending →
  pull versions past the cursor; at-least-once with server-side dedup,
  exponential-backoff background task, transparent full resync that
  preserves and replays pending changes), and a mountable server router
  (`POST /sync/push` + `GET /sync/pull` via `AppBuilder::nest`) over
  Postgres shadow tables with idempotent DDL (`PgSyncBackend`) or an
  in-memory backend for tests. Conflicts are settled server-side by a
  pluggable `ConflictResolver` (default: last-write-wins on the conflicting
  writes' `updated_at`; exact ties break to the lexicographically greater
  device id); resolved rows get a new version so every device converges.
  Postgres pushes serialize under an advisory lock (in-order version
  visibility for pulls; concurrent first-inserts of one pk engage the
  resolver), pull sessions carry a session-start cursor so multi-page
  catch-ups survive tombstone GC, completed catch-ups land the cursor at
  the GC horizon and prune GC'd local tombstones, `already_applied` acks
  return the originally assigned version, push/pull request sizes are
  bounded server-side, dedup records are GC-able via
  `SyncBackend::gc_applied`, and `SyncConfig::bearer_token` authenticates
  the engine against an auth-guarded `/sync` mount. Zero new
  dependencies — builds on the `db` and `http-client` features already in
  the graph. [no-plugin]
- **generator:** `autumn generate tauri-mobile --offline-sync` (issue #1508)
  wires the offline-sync engine into the mobile scaffold: the shell opens a
  `SyncStore`-backed SQLite database in the app sandbox (exported as
  `AUTUMN_SYNC__DB_PATH`), runs a background `SyncEngine` against
  `AUTUMN_SYNC__REMOTE_URL` (30 s interval, exponential backoff while
  offline, plus an immediate pass on `RunEvent::Resumed` when the app
  returns to the foreground), and the app crate gains a default
  `offline-sync` feature and a `/sync` router mounted in the extracted
  `serve()` — only when the app's resolved config has a database URL (e.g.
  `AUTUMN_DATABASE__URL`), and with log-and-continue schema DDL, so the
  same binary boots fully offline on a device (no database at all) and
  serves sync on the server. Without the flag the emitted scaffold is
  byte-identical to before (pinned by a golden snapshot test). Docs:
  architecture, change tracking, tombstoning/GC, conflict resolution, and
  an airplane-mode walkthrough in
  [docs/guide/tauri-mobile-offline-sync.md](docs/guide/tauri-mobile-offline-sync.md).
  [no-plugin]

### Documentation

- **plugin:** refresh the Claude plugin to current framework state. Adds
  prominent `#[state_machine]` coverage (attribute syntax, generated
  `can_transition_{field}_to` / `transition_{field}_to`, guards, and a
  `before_update` example replacing hand-rolled status validation), a
  "Prefer framework idioms over raw Diesel/Axum" steering table, the
  generated-repository method surface (pagination, bulk ops, read routing,
  hooks), `Db::tx_with`/`TxOptions`, jobs additions (uniqueness/concurrency,
  named queues, tracked jobs), events/listeners, cache stampede protection,
  view widgets + `WIDGETS_CSS`, sharding, `autumn serve`, `autumn destroy`,
  the `references`/`enum{...}`/`decimal`/`:unique` generator DSL, scoped
  tokens, observability defaults, and load shedding. Features merged on
  trunk-dev but absent from the published 0.5.0 crates are explicitly marked
  "(unreleased)". Follow-up: once #1587 (`form_for`) and #1592
  (`find_in_batches`) merged, their in-flight/do-not-use flags were replaced
  with real coverage — `form_for`/`FormModel`/`FieldControl` (builder methods,
  derived control mapping, checkbox/datetime/serde-rename decode contracts,
  scaffold `{snake}_form_for` emission) and
  `find_in_batches`/`find_each`/`autumn_web::batches` (keyset semantics,
  retryable errors, scoping/routing inheritance, sharding limits) — each
  marked "(unreleased)". Fixes stale claims (foreign keys/unique "not in the DSL", the
  removed `--test repo_hygiene` target) and documents that published 0.5.0
  repositories have no pool constructor (`with_pool_untracked` is
  trunk-dev-only).

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

