# Changelog

All notable changes to the Autumn framework will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **cli:** `autumn destroy` no longer reports `Diverged` for an untouched file
  whose generator template changed since the project was generated (issue
  #1835). `generate` now records a digest of every file it owns in
  `.autumn/generated.toml`, and `destroy` accepts a file matching either that
  digest or the current render. A real edit matches neither and is still
  refused without `--force`, and the applied-migration guard is unchanged.
  Commit the manifest — it is the baseline a later checkout compares against.
  A project generated before the manifest existed keeps the previous
  behaviour: compare against the current render only, `--force` to override.
  Each entry also records the inputs that produced it — the command's
  arguments, a fingerprint of the `autumn.generate.toml` they resolve from, and
  the resolved database backend — so the digest counts only when all three
  match. `autumn destroy model Post` after `autumn generate model Post
  title:String` is still refused; editing the recipe, or moving the project
  between SQLite and Postgres, drops the baseline rather than trusting it; and
  files written by `autumn new --starter`, which uses the same machinery, are
  never a generator's to delete.
  A side effect of the digest being taken over LF-normalised text: a CRLF
  checkout of a generated file (`core.autocrlf`) no longer reads as an edit,
  whether or not a manifest entry backs it.

- **plugin-sandbox:** three consequences of #1632 that an existing sandbox
  embedder will notice. `SandboxManifest` gains `grants` and `quotas` fields, so
  a struct literal over it needs two more lines — prefer `SandboxManifest::parse`
  and edit the public fields of what comes back, which is unaffected;
  `SandboxManifest::grants(cap)` is renamed `is_granted(cap)`, because `grants`
  is now the field holding what each capability is scoped to. The per-request
  host footprint gained the capability reply queue and the audit ledger, so a
  manifest tuned to the previous ceiling may now be refused at load with
  `LimitOutOfRange` on "the per-request host footprint × max_concurrency" —
  lower `max_concurrency` or `max_response_bytes` (an otherwise-default manifest
  still admits `max_concurrency = 16`). And the `autumn plugin-check` gating
  line for a sandboxed plugin now reads "no session, auth, filesystem or
  environment capability" and names the grant scopes, rather than claiming no
  database or network capability over a manifest that holds one — and it is
  carried by a `capability-grants` check present on every sandboxed report,
  rather than only by the sensitive-surfaces check, which a plugin whose prefix
  is not named like `/admin` never reaches. Non-breaking
  for the stable surface: all of it is behind the non-default `plugin-sandbox`
  feature, which `STABILITY.md` places outside SemVer.

### Added

- **examples:** `examples/react-graphql`, a TypeScript React single-page app
  on an Autumn backend that talks GraphQL through a plugin, against a real
  Postgres `#[model]`. Two files carry the point. `src/graphql_plugin.rs` is a
  generic `GraphqlPlugin<Q, M, S>` that adapts any `async-graphql` schema onto
  an app — `AppBuilder::nest` for the raw router (with the schema as
  router-local `Extension` state, so two schemas can share root types at two
  paths), `declare_plugin_routes` so `autumn routes` and the audit gate see it,
  per-execution injection of `AppState` into the GraphQL context, a
  `PluginContract`, and a `GET` transport that refuses non-query operations
  with `405`. `src/notes.rs` shows what that buys: every resolver builds the
  generated `PgNoteRepository` from the pool on `AppState`
  (`with_pool_untracked`, the constructor for code with no request), so
  `#[normalize(trim)]`, the model's `#[validate]` rules, and the repository's
  `MutationHooks` (`before_create` validation, a `before_delete` rule refusing
  to delete a pinned note) apply identically to a GraphQL mutation and
  the generated `api = "/api/notes"` REST handlers mounted beside it (the
  `on_startup` seed runs once across instances, on one connection under a
  transaction-scoped advisory lock). `AutumnError`s become GraphQL field errors carrying the
  HTTP status in `extensions.status`. The plugin serves `POST /graphql`, the
  GraphQL-over-HTTP `GET` form, and `GET /graphql/sdl`, whose output is
  drift-tested against a committed `schema.graphql` the TypeScript types are
  written against. The Vite/React 19 bundle is committed under `static/app/`
  (fixed file names, no hash) and served by the standard `/static` mount under
  the default `script-src 'self'` CSP, so `cargo run` needs no Node toolchain;
  `autumn build --embed` bakes it into the binary (`embed_static!` +
  `.embedded_static`, behind the crate's `embed-assets` feature); `npm run dev`
  proxies `/graphql` to the Rust server for hot-reload work. Tested in two
  tiers plus a smoke: `TestApp` tests with no Docker (shell, SDL drift,
  `plugin_conformance::run_conformance`, error mapping, the `GET` guard,
  two plugins at two paths), `TestDb` testcontainer tests that apply the
  example's real embedded migration (rows, hooks, normalisation, validation,
  REST/GraphQL parity), and a Chromium smoke that drives the real binary
  against a testcontainer Postgres through a query and a form mutation.
  Cataloged as a supported example.

- **cli/generate + sqlite:** the **DB-backed sessions store now runs on SQLite**
  (#1908). The tracked-sessions store `autumn generate auth` scaffolds bounded
  its query functions by `diesel::pg::Pg`, which rejects the SQLite
  `RuntimeConnection`, so the store did not compile on a SQLite app. Every such
  bound in the generated auth surface (the four session revoke/list functions and
  the seven remember-me chain functions) is now
  `::autumn_web::RuntimeBackend` — the alias that resolves to `diesel::pg::Pg` by
  default and `diesel::sqlite::Sqlite` under the `sqlite` feature — so the
  scaffolded store compiles and runs on whichever backend the app selected.
  Postgres behaviour is unchanged; the alias resolves to the same backend it
  already used. The scaffolded `docs/guide/session-management.md` also emits its
  operator SQL in the app's own dialect: the stale-row sweep is
  `datetime('now', '-90 days')` on SQLite (was Postgres-only `NOW() - INTERVAL`),
  and its retrofit `CREATE TABLE` is now rendered from the same helper as the
  migration, so the two cannot drift. The scaffolded `docs/guide/oauth.md`
  documents its `oauth_identities` schema in the app's dialect for the same
  reason. A new `sqlite_tracked_sessions` test runs the generated store's shape
  against a real, file-backed SQLite database over a multi-connection pool —
  login tracking, the revocation gate (including a revoke committed on another
  connection), the `UNIQUE` digest guard, `last_seen_at` refresh, rotation
  rebinding, the three revoke paths, the documented retention sweep across both
  timestamp encodings, and `ON DELETE CASCADE` on account deletion. A
  `sqlite_test_targets_are_ci_named` hygiene test now fails the build if any
  `sqlite`-gated `[[test]]` target is missing from the CI job that names them,
  so a future target cannot ship dark. Guide:
  `docs/guide/sqlite-in-production.md`.
- **cli:** dependency advisories and policy reach the dev loop (issue #1633).
  `autumn doctor` gains a `dependencies` check that grades the app's lockfile
  against its own `deny.toml` — the same policy file, waiver store, check list
  and auditor that #1600's CI gate runs, so a local verdict predicts the CI
  verdict. Two differences remain and are reported rather than hidden: CI pins
  cargo-deny 0.20.2 while a local run uses whatever is installed, and CI
  fetches the advisory database every run while doctor reads local data and
  names its age. Each finding reports its advisory or violation id, severity,
  crate and title; a waived finding shows as waived and never fails, and a
  tree with nothing live is exactly one line. Severity is consequence: what the
  policy denies grades high or critical (CVSS v3 separates the two), what it
  warns about grades low or medium. `autumn dev` reports only findings the
  policy **denies** — the ones that turn CI red — so a clean tree, a fully
  waived tree and a tree with only warn-level findings all add nothing to its
  output; a critical advisory gets a startup banner. The audit is read after
  the initial build, so a cold start never waits on it. Neither command
  fetches: both run `cargo deny --offline`, doctor warns once when the advisory
  data is over 7 days old, and a missing auditor or database is a **pass** that
  reads `not evaluated` — never a silent pass, and never a warning that would
  make `autumn doctor --strict` red on every machine that has not installed an
  optional tool. `autumn new` now scaffolds `[licenses]`, `[bans]` and
  `[sources]` into `deny.toml` as commented, quiet defaults, and the generated
  CI workflow derives its check list from the sections that file declares — in
  every TOML spelling, by the same rule doctor uses — so uncommenting one
  widens the local check and the CI gate together. See
  docs/guide/supply-chain.md.
- **autumn-macros:** every macro's generated code now resolves the
  `autumn-web` crate path via [`proc-macro-crate`](https://docs.rs/proc-macro-crate)
  instead of a hardcoded `::autumn_web` (issue #1828), so a downstream crate
  that depends on `autumn-web` under a renamed Cargo key (`web = { package =
  "autumn-web" }`) can use `#[get]`, `#[model]`, `#[repository]` and every
  other Autumn macro with no changes. For the rarer case of hosting two
  differently-keyed `autumn-web` versions in one crate at once (e.g.
  mid-upgrade), where automatic detection is ambiguous, every attribute macro
  additionally accepts an explicit `crate = "..."` override, e.g.
  `#[get("/x", crate = "autumn_web_05")]`.

- **plugin-sandbox:** the capability vocabulary grows past request handling
  (issue #1632). A sandboxed plugin's manifest may now ask for `kv`,
  `http-outbound`, `db`, `jobs` and `render` beside `http-request`, and a new
  `[grants]` table says what each is scoped to — hostnames, plugin-owned tables,
  job types, render slots — with per-request `[quotas]` an operator can tune.
  The guest asks over the NDJSON channel it already answers on
  (`{"op":"call","call":"kv-get",…}` → `{"op":"call_result",…}`), so **a plugin
  granted every capability imports exactly what a plugin granted none imports**
  and the #1609 escape corpus keeps proving what it proved. Scoping is by
  derivation rather than by check: the guest names a logical key, table, host or
  job type and the host derives the physical one from the manifest and the
  active tenant, so cross-tenant and host-table access are unspellable rather
  than refused. Render hooks return a fragment *tree* the host renders, not HTML
  it sanitises — no parser, so no parser differential — and a hook that traps,
  overruns its fuel or emits a tag the renderer will not produce omits the
  fragment rather than taking the page down with it. Every call, allowed or
  refused, lands
  in a bounded per-plugin activity log that answers "what did this plugin do in
  the last hour" from one surface: hosts called, KV/DB usage, jobs enqueued,
  denials and quota hits, recorded as shapes and never as values — and, when a
  plugin outruns its own ledger, a line saying every count below it is a floor.
  The `jobs` capability **enqueues**; running the result is the host's, and this
  wire version has no frame for delivering a job back into a guest.
  `autumn plugin inspect --against <installed-artifact>` reviews an upgrade as
  an upgrade — capabilities, grant lists, quotas, **routes and resource
  ceilings**, since a new route is an endpoint nobody approved and a raised
  `fuel` is authority that touches no capability name — printing exactly what
  grew and exiting non-zero when anything did. `autumn plugin inspect`
  (text and JSON) now carries the grant lists and quotas, and stops printing
  "no database access" over a manifest that was just granted `db`. Fifteen-plus
  adversarial corpus of cross-capability escape attempts runs end-to-end through
  the real interpreter in `tests/integration/plugin_sandbox_capabilities.rs`.
  Every result the guest can size is bounded before it is built rather than
  after: a row carries at most 256 KiB across its columns (checked on the way
  *in*, so a stored row can always be read back), one `db-get`/`db-query` answer
  carries at most 512 KiB and says `"truncated": true` when that cut it short,
  and the budget travels into `PluginStore::query` so a store never materialises
  what the reply would discard — with an `after` cursor so a page that was cut
  can actually be continued, and a query filter refusing `row_id` because
  stripping it would turn "the row with this id" into "every row this tenant
  has". The outbound response-header ceilings travel into `OutboundRequest`
  beside `max_response_bytes`, the render context is bounded before it is cloned
  onto a worker, and `CacheKvStore` uses the serde-aware cache API so a plugin's
  KV survives on a cross-replica backend rather than silently storing nothing. The shipped `MemoryJobSink` has a finite default
  depth and no unbounded spelling — this slice ships no consumer that drains it.
  The activity log counts what it evicts as well as what a per-request ledger
  overflowed, and both are timestamped and windowed like ordinary events, so a
  "last hour" neither presents the last twenty seconds as the hour nor carries a
  lifetime total into a window the calls were not in. See
  `docs/guide/sandboxed-plugins.md`. **Non-breaking**: everything here is behind
  the non-default `plugin-sandbox` feature, which `STABILITY.md` already places
  outside SemVer, and a first-slice manifest parses and runs unchanged —
  `[grants]` and `[quotas]` both default, and an unknown capability name is
  still a refusal rather than a silently dropped grant. The only source-level
  change for an embedder is that `SandboxManifest` gains `grants`/`quotas` and
  `SandboxOutcome` gains `activity`, so a struct literal over either must name
  the new fields.
- **deploy:** `autumn deploy check` now prints the same config-manifest signal
  `autumn deploy up` already prints (#1952 check/up parity) — a confirming
  line naming the `autumn.toml` (and, when present, `autumn-<profile>.toml`)
  that will be uploaded, or the loud "no autumn.toml found" warning when the
  project has none. Before this, an operator relying on `deploy check` as the
  documented way to catch a broken deploy before touching the server had no
  signal at all that the deployed app would silently run built-in defaults —
  they only found out once they actually ran `up`. Purely informational: it
  is not a graded preflight check and never affects `check`'s exit code. See
  `docs/guide/deployment.md`'s "Your `autumn.toml` is deployed alongside the
  binary" section for the full behavior this closes out. [no-plugin]

- **testing:** a real-ACME end-to-end test drives the ACME order state
  machine (order → HTTP-01 → finalize → issue) against a real, independently-
  implemented ACME server — [Pebble](https://github.com/letsencrypt/pebble),
  run as a Docker container via `testcontainers`' `host-port-exposure` tunnel
  — asserting a genuine, parseable, not-yet-expired certificate is obtained
  and hot-swapped into the live TLS resolver. Every other ACME test drives
  the same `AcmeRenewalTask` against an in-process fake CA; this closes the
  test-depth gap explicitly deferred from #1608/PR #1858 (issue #1863), so a
  protocol-level regression (challenge ordering, the finalize payload shape,
  polling) that the fake CA cannot see would still be caught. Wired into a
  dedicated, Docker-gated CI step separate from both the fast ACME lane (no
  Docker/network) and the general Docker-dependent-tests sweep. Test-only
  coverage, no new agent-facing surface. [no-plugin]
- **graph:** a queryable architecture graph derived from the app's own macros
  (issue #1747). Autumn already declares every architectural element through
  proc-macros it owns, but none of that survived expansion as something you
  could ask a question of. It does now: `#[route]`/`#[static_get]`,
  `#[model]`, `#[repository]` and `#[job]`/`#[scheduled]`/`#[task]` each
  register a node, and the framework assembles them into a typed graph at link
  time — nodes for every declared element (each route carrying its mounted path
  and declared auth requirement, each model its table), edges for
  repository→model declarations, for the repository a handler takes as an
  extractor, and for every model, table or raw-SQL table name a route or job
  body mentions. `autumn graph show|touches <NAME>|impact <NAME>` answers
  against it — "which routes and jobs touch `posts`", "what does changing
  `Post` break" — and `--check` fails the build when a declared element or an
  edge quietly disappears. The same graph is served from `/actuator/graph`
  (sensitive-gated, like `/env`) so a running single binary can answer
  questions about itself with no side file to go stale. Because node identity
  comes from the declaration, nothing can fall out silently:
  `examples/reddit-clone/tests/architecture_graph.rs` censuses the reference
  app's *sources* for every declaring attribute, runs the binary's own graph
  dump, and fails when the two disagree — including a hand-verified
  ground-truth list that pins `impact Post` to total recall over both access
  styles the app uses (repository extractors and raw Diesel), the
  `#[repository(api = …)]` auto-API routes, and a scheduled task that reaches
  the table only through `sql_query("UPDATE posts …")`. Edges from a route or
  job are a name-based derivation over that item's own tokens, deliberately
  biased toward over-reporting; every edge carries its provenance
  (`declaration`/`signature`/`body`) and the document carries the derivation's
  limits, so it cannot be read as more than it is. See
  `docs/guide/architecture-graph.md`.
- **macros:** `#[autumn_web::main]` takes optional arguments that reach the
  Tokio runtime it builds. Previously the attribute discarded its argument
  list entirely (`_attr`), so the only way to size a worker pool, name the
  worker threads, or install an `on_thread_start` hook was to abandon the
  macro and hand-roll `main` — which also meant re-implementing the two
  compile-context side effects it exists for, the `autumn.toml` root and the
  `/actuator/info` build provenance, both of which must be expanded in the
  *app* crate to be correct. The attribute owns the
  `tokio::runtime::Builder` call, so the knobs are now arguments on it:
  `flavor` (`"multi_thread"`, the default, or `"current_thread"`),
  `worker_threads`, `max_blocking_threads`, `thread_name`,
  `thread_stack_size`, `thread_keep_alive` (a duration string such as
  `"30s"`, the same spelling `#[throttle(per = ...)]` accepts), and
  `configure` — the path of a `fn(&mut tokio::runtime::Builder)` that runs
  last, after the declarative arguments, as the escape hatch for every
  `Builder` method the list does not name (`on_thread_start`,
  `global_queue_interval`, …) and as the way to override one of them. The
  numeric arguments take arbitrary expressions rather than only literals, so
  `worker_threads = std::thread::available_parallelism().map_or(4, |n| n.get())`
  is as valid as `worker_threads = 4`.

  With no arguments the expansion is byte-for-byte what it was before, so
  nothing changes for an app that does not opt in. What *does* change is that
  an argument list is no longer silently dropped: a typo (`worker_thread`), a
  repeated argument, an unknown `flavor`, a literal `0` where tokio would
  panic at startup, a malformed `thread_keep_alive`, and a `worker_threads`
  paired with `flavor = "current_thread"` (where the runtime has no worker
  pool to size and the value would do nothing) are each a compile error naming
  the problem. The `configure` path is bound to a typed `fn` pointer before it
  is called, so a wrong signature is reported against the `configure = ...`
  the user wrote rather than inside the expansion. Covered by expansion unit
  tests in `autumn-macros/src/main_macro.rs`, by two compile-fail fixtures for
  the refusals (`tests/compile-fail/main_unknown_runtime_arg.rs`,
  `tests/compile-fail/main_worker_threads_current_thread.rs`), and — because
  trybuild `pass` fixtures are compiled *and run* — by two behavioral ones
  (`tests/compile-pass/main_runtime_args.rs`,
  `tests/compile-pass/main_runtime_current_thread.rs`) that boot the tuned
  runtime and assert the settings actually landed on it: the blocking thread
  carries the requested `thread_name`, and the `configure` hook's
  `on_thread_start` has fired. Documented in the getting-started guide under
  "Tuning the Tokio runtime".

- **macros:** closes out the residual long tail of partial-patch (`Patch<T>`)
  update validation left after #1719/#1742/#1778/#1801 (issue #1751).
  `must_match` — like `custom`, `ip` on `Option<_>` fields, and
  `does_not_contain` before it — is now behaviorally proven, not just
  asserted, to be enforced on the update path via merged-model validation
  (`from_patch`): `tests/integration/validate_merged_model.rs` gained a
  dedicated cross-field `password`/`password_confirm` case showing the patch
  struct alone stays create-only while the merged model correctly rejects a
  mismatch and accepts a match. Investigating the last item, `nested`,
  surfaced a real, previously-undiscovered defect rather than a mere test gap:
  `validator_derive`'s `nested` codegen calls a field's value with bare
  `(&field).validate()`, which collides with this crate's own `ValidateExt`
  (`autumn_web::prelude::ValidateExt`, a blanket `impl<T: validator::Validate>
  ValidateExt for T` also named `validate`) whenever a struct with a
  `#[validate(nested)]` field is declared in a module that ALSO imports the
  prelude — a cryptic `E0034: multiple applicable items in scope` pointing
  into the derive expansion, on **create as much as on update**, and equally
  possible on `#[autumn_web::model]` structs (which forward `#[validate(...)]`
  verbatim) as on hand-rolled ones. The collision is scoped to the struct's
  own defining module, not any downstream consumer's — proven with a
  compile-fail/compile-pass fixture pair
  (`tests/compile-fail/validate_nested_collides_with_validate_ext.rs` /
  `tests/compile-pass/validate_nested_without_validate_ext.rs`) — and a
  derive/attribute macro cannot see the rest of its enclosing module's `use`
  statements, so `#[model]` cannot detect or refuse it at expansion time.
  `ValidateExt`'s doc comment now documents the hazard and its scope, with the
  workaround (keep the struct's own module free of that import, or use
  `#[validate(custom(...))]`). `credit_card`/`non_control_character` remain
  correctly out of scope: they are not in this workspace's enabled `validator`
  feature set (only `derive`, not `card`/`unic`), so no model can use them
  today regardless of the update path.

- **A CI gate for `AUTUMN_*` config keys named in the docs [no-plugin]:**
  nothing here is agent-facing — it's a CI/docs-harness addition, not new
  framework surface (`scripts/check-docs-config.sh`, wired into the docs-only
  job in `.github/workflows/ci.yml`). The corpus already gates the link a
  reader clicks (`check-docs-links.sh`) and the command they run
  (`check-docs-cli.sh`); this gates the third thing they copy off a page, and
  the only one of the three that fails **silently**. A wrong link 404s and a
  wrong command exits 2, but a wrong environment variable is simply not read:
  the process starts, the default stands, and nothing anywhere reports that
  the override was ignored. `autumn check --config` and `server.strict_config`
  reject an unknown key in `autumn.toml`, but neither sees the env layer — an
  override is applied by name at load time or not at all — so
  `AUTUMN_DATABASE_URL` (one underscore, the pre-0.2 spelling) beside a
  production Postgres URL reads exactly like a working line, and the app comes
  up on the default database.

  The gate asks whether the runtime **reads** a name, not whether a config key
  is spelled like it. Those are different sets: the env layer is written field
  by field (`parse_env(env, "AUTUMN_LOG__LEVEL", …)`), so a TOML key with no
  override of its own has no environment spelling at all — `openapi.enabled`
  is a real schema leaf and `AUTUMN_OPENAPI__ENABLED` is read by nothing, as
  are 90 of the 397 leaves. So a name resolves when something in the tracked
  non-markdown tree **binds** it (`const CANARY_ENV: &str = "AUTUMN_CANARY"`)
  or **reads** it through an env accessor, or when it matches one the runtime
  **builds** (`format!("AUTUMN_AUTH__OAUTH2__{upper}__CLIENT_ID")` — the
  filled-in segment open, the rest exact, which is the runtime's own
  behaviour). That covers the four subsystems outside `AutumnConfig`:
  `autumn-search` and `autumn-media-plugin` layer their own overrides in their
  own crates, `autumn-cli` owns `[dev] watch_dirs`, and `AUTUMN_SYNC__*` is
  read by the Tauri shell the CLI generates.
  `autumn/tests/fixtures/schema_keys.snapshot` — the same schema walk that
  backs strict unknown-key validation, already kept honest by
  `schema_keys_snapshot_guard`, so nothing needs regenerating and no Rust
  toolchain is required — bounds the one open-ended template
  (`…SHARDS__{i}__{field}`) and checks declared config *paths*.

  It also gates the hand-maintained 142-row `AUTUMN_* -> config path` table in
  `config.rs`'s module docs, the mapping readers meet on docs.rs. Each row
  makes two claims, checked against their own truth: the path must exist in
  the schema, the variable must be one the runtime reads, and the two must
  agree — so neither a row edited on one side nor a row publishing an override
  that sets nothing gets through. Any row shaped like a mapping that the
  checker cannot parse is reported rather than skipped.

  Prose that names a family (`AUTUMN_ALERTS__*`,
  `AUTUMN_MEDIA__<TABLE>__<FIELD>`), pages teaching the naming rule
  (`AUTUMN_SECTION__FIELD`), reader-chosen names
  (`access_key_id_env = "AUTUMN_OFFSITE_ACCESS_KEY_ID"`) and identifiers a page
  declares in its own example code (`pub const AUTUMN_SOURCE: &str = …` in
  `docs/guide/wasm-islands.md`) are recognised as such rather than waived —
  the last of those only inside that page's Rust fences, and only where the
  occurrence is not a string literal, so a snippet that both declares a const
  and calls `env::var("…")` on the same spelling still has the call checked. A
  malformed name — lower case outside a placeholder, or a dangling separator —
  is reported rather than skipped, since a spelling that matches nothing is a
  claim the reader cannot be warned about.

  The corpus is the pages a reader lands on, defined identically here and in
  `check-docs-cli.sh`: the guides, the migration notes, the root `README.md`
  and its siblings, `docs/plugins.md`, the markdown templates written into
  every scaffolded project, and each example's `README.md` — the page GitHub
  renders when a reader follows one of the example links in the README table,
  and where `export AUTUMN_SECURITY__SIGNING_SECRET="$(openssl rand -hex 32)"`
  and friends are copied from.

  The baseline run found **0 defects** across 681 occurrences on 193 pages:
  the reader-facing corpus was already accurate, and the gate is here to keep
  it that way. One occurrence is waived in place beside the passage that needs
  it — a migration guide's `rg` pattern for a key that release removed.

- **A CI gate for the "local development" install path [no-plugin]:** nothing
  here is agent-facing — it's a CI/test-harness addition, not new framework
  surface (`local-dev-quickstart` job in
  `.github/workflows/quickstart-gate.yml`). `cargo install --path
  autumn-cli` from a source checkout, then `autumn new`, is a first-class path
  in README.md and `docs/guide/getting-started.md`, but nothing in CI built
  that exact pairing — a source-built (trunk-dev) CLI's scaffold against the
  `autumn-web` actually **published** on crates.io. `autumn new` pins the
  scaffolded `autumn-web` dependency to the CLI's own `CARGO_PKG_VERSION`,
  which is frozen at the last release tag between releases (this repo's
  policy: never bump the workspace version except at an explicit release), so
  a source-built CLI can carry unreleased `autumn-web` API changes while
  reporting the same version as the last published crate — and `autumn
  doctor`'s `version_compat` check, a plain version-string comparison, cannot
  see the difference. Confirmed live: commit 76c56b1 widened
  `inject_consent_banner`'s `csrf_cookie_name` from `&str` to `Option<&str>`
  and correctly updated the in-tree scaffold template, but three days later
  (no release cut since) a freshly `autumn new`-ed project fails its first
  `cargo build` against published `autumn-web` 0.7.0 with `error[E0308]`,
  while `autumn doctor` still reports the versions as matching. The new
  `generated_project_compiles_against_published_autumn_web` test
  (`autumn-cli/tests/e2e.rs`) reproduces this without the
  `[patch.crates-io]` override the existing (in-tree-pairing) e2e test
  applies, and the new job runs it on every `trunk-dev` push and the
  existing daily schedule — skipped on `workflow_dispatch` release-candidate
  gating, since it tests trunk-dev source drift rather than the dispatched
  candidate and would otherwise false-block a healthy release
  (`docs/release-checklist.md` step 7). This is a harness addition, not a
  fix: the underlying drift is real and current, and the job is expected to
  go red between releases until a maintainer addresses `autumn doctor`'s
  blind spot or the next release ships.

- **Continuous SQLite replication with point-in-time restore (#1628):** the
  zero-ops SQLite tier (#1614) had snapshot backups (#1595/#1619) and nothing
  finer, so a dead VPS cost everything written since the last snapshot — hours.
  A running app now ships its write-ahead log to an offsite destination
  continuously, from inside the process it already runs: no sidecar to install
  and supervise, no external tools, no new credential conventions.

  ```toml
  [replication]
  enabled = true

  [replication.s3]
  bucket = "myapp-replicas"
  region = "auto"
  endpoint = "https://<account-id>.r2.cloudflarestorage.com"
  access_key_id_env = "AUTUMN_REPLICA_ACCESS_KEY_ID"
  secret_access_key_env = "AUTUMN_REPLICA_SECRET_ACCESS_KEY"
  force_path_style = true
  ```

  The contract is **at most `rpo_secs` (default 10) of committed writes lost**
  when the machine is destroyed. Only complete transactions ship — a segment
  always ends on a commit boundary — and a checkpoint is attempted only once
  everything in the WAL is already offsite, so an unreachable destination costs
  disk, never data. Steady-state upload is the size of your *writes*, not of your
  database: a checkpoint opens the next WAL index inside the current generation,
  and a full base snapshot is taken once per `snapshot_interval_secs` (hourly by
  default), which is also what bounds how much WAL a restore replays.

  `[replication]` reuses #1619's destination conventions — its own config
  section, profile overlays, `AUTUMN_REPLICATION__*` overrides, env-var-indirected
  credentials, and a refusal to share the app's blob-storage bucket + endpoint
  without `allow_shared_bucket = true`. A `path` destination replicates to a
  directory (second disk, NFS/SSHFS mount, bind-mounted volume) instead.

  Recovery is one command on a fresh box that has only the binary, `autumn.toml`
  and the credentials:

  ```bash
  autumn db replica status                     # how fresh is it?
  autumn db replica restore --force            # latest state, fresh box
  autumn db replica restore --timestamp 2026-09-02T14:29:00Z --force --overwrite
  ```

  Restore **refuses** rather than best-efforts: a hole in the segment sequence, a
  payload whose SHA-256 does not match, a segment that does not continue the
  previous one, or a rebuilt database that fails `PRAGMA integrity_check` is an
  error — handing SQLite a damaged WAL would have looked like a clean restore
  that was merely missing the last few minutes. Nothing is published until those
  checks pass, and the same production guard / `--force` protocol as #1595
  applies — `--force` for the production-profile guard, and a separate
  `--overwrite` to replace a database file that is already there, so a drill that
  always passes `--force` cannot silently destroy one.

  Lag, the current generation and the last successful verification are on
  `/actuator/health` under the `sqlite-replication` indicator and in
  `autumn db replica status`. Verification is a **real restore** on an interval,
  not a checksum, so "uploaded" is never mistaken for "restorable"; a
  verification failure or lag beyond three RPOs takes the indicator `DOWN`, which
  the existing #1610 alerter escalates on every configured channel. See
  [SQLite in production → Durability](docs/guide/sqlite-in-production.md#durability-continuous-replication-and-point-in-time-restore).

  **Breaking:** `AutumnConfig` gains a public `replication` field, so a
  struct-literal construction needs `..AutumnConfig::default()` — see
  [the migration guide](docs/migrations/next.md#config-autumnconfig-gains-a-replication-field).
- **Wildcard certificates via DNS-01 for tenant subdomains (#1620):** autumn
  already routes a tenant per subdomain and ships a multi-tenant SaaS starter,
  but the ACME support in #1608 was HTTP-01 only — and no CA validates a
  wildcard identifier over HTTP-01. A `*.myapp.com` deployment therefore meant
  one certificate order per tenant (rate limits, and a cold-start stall on
  tenant *N*'s first request) or going back behind Caddy/nginx, undoing the
  single-binary story. A new `[server.tls.acme.dns]` section answers every
  authorization over DNS-01 instead, so one wildcard covers every tenant that
  exists and every tenant that ever will:

  ```toml
  [server.tls.acme]
  domains = ["myapp.com", "*.myapp.com"]
  contact_email = "ops@myapp.com"
  directory = "production"

  [server.tls.acme.dns]
  provider = "cloudflare"
  ```

  Onboarding a tenant after that costs zero certificate work: no issuance, no
  restart, no config change.

  - **Providers.** `cloudflare` (scoped API token) and `route53` (SigV4), plus
    `exec` — an argv-array hook program run as
    `hook present|cleanup <fqdn> <value>`, which reaches RFC 2136 through
    `nsupdate`, a registrar CLI, or a webhook shim. No shell is involved, so a
    challenge value can never be read as shell syntax. The hook's `stderr` is
    read through a bounded buffer and scrubbed before it is published: both the
    credentials autumn holds and any the *inherited environment* holds under a
    credential-shaped name, since an `exec` hook authenticates itself from that
    environment and a `set -x` trace would otherwise republish its token.
    Each provider resolves the zone for the whole challenge name rather than
    caching by suffix, so a cached parent zone can never shadow a separately
    delegated child.
  - **Secrets.** The section names a credentials-store *key*, never a token:
    there is no config field that could hold one, and the section rejects
    unknown keys, so an `api_token` written into `autumn.toml` is a startup
    error naming it rather than a plaintext secret. Credentials come from the
    encrypted store (`autumn credentials edit`) or the `AUTUMN_ACME_DNS_*`
    environment variables, and are held in a type that renders as `<redacted>`
    so they cannot reach a log, an error message, or actuator output.
  - **Propagation.** After publishing, autumn waits until every record is
    visible before telling the CA to validate. The probe goes to each challenge
    zone's *own* authoritative nameservers, discovered per order through the
    configured resolvers — probing a public recursive right after the write
    plants a negative-cache entry (RFC 2308; 900s on Route 53, 1800s on
    Cloudflare) that outlives the propagation budget and can never clear. A
    multi-domain order discovers nameservers per zone, since one zone's servers
    never answer for another's names. The configured resolvers stay the fallback
    for any zone whose authoritative set cannot be discovered. The wait is
    bounded (`propagation_timeout_secs`, default 300) and its timeout names the
    exact record, the value that never appeared, and the resolver that never saw
    it. Challenge records are removed after the order finishes, including when
    it fails.
  - **Two records, one name.** An apex + wildcard order publishes two different
    values at `_acme-challenge.<domain>`; every provider appends a value and
    deletes by `(name, value)` rather than replacing the record set. Providers
    whose unit of write is the whole record set rather than one record —
    Route 53's `ChangeResourceRecordSets` — apply every value sharing a name in
    a single change, because two sequential read-modify-writes race: the second
    read can still return the pre-change values and write back only its own.
    Such a write also carries the existing set's **TTL** back out unchanged —
    the name can be shared with another ACME client, so autumn's own 60s
    challenge TTL applies only to a set it creates, and Route 53 rejects a
    `DELETE` whose TTL does not match the live set, which would otherwise leave
    cleanup unable to remove anything.
  - **Lifecycle.** Renewal, persistence, staging selection, hot-swap and health
    are #1608's, unchanged. A failed issuance or renewal now also raises
    #1610's `scheduled_task_failure` operator alert for `acme-renewal` — weeks
    before expiry, thanks to the renew-before window — and clears it on the next
    success. The `acme` health indicator reports `challenge` and `dns_provider`.
  - **`autumn doctor`.** Three new checks: `acme_dns_credential` (the provider
    credential is readable and complete), `acme_dns_propagation` (`--online`:
    public DNS can answer for `_acme-challenge.<domain>` at all), and
    `acme_tenancy_domain` (`[tenancy] base_domain`'s subdomains are actually
    covered by the configured certificate). An unreachable `:80` is now a Warn
    rather than a Fail when DNS-01 is configured, since the CA never connects to
    it, and a `*.` entry is probed as the base domain it covers.
  - **Still single-host.** DNS-01 retires HTTP-01's per-process token map, but
    it does not distribute certificates: the store is local disk, so only the
    replica holding the renewal lease has the issued certificate. A distributed
    `[scheduler]` backend therefore still warns at startup — now naming the
    certificate store rather than the token map.

  See the [TLS guide](docs/guide/tls.md#wildcard-certificates-via-dns-01-servertlsacmedns)
  and the [deployment walkthrough](docs/guide/deployment.md#subdomain-per-tenant-wildcard-https-on-a-vps).

  **Breaking:** (`acme` feature only) `AcmeRenewalTask` gained the public fields
  `dns` and `recovery`, and `AcmeConfig` gained `dns`; code that constructs
  either literally must add them (`None` preserves today's HTTP-01 behavior).
  The new *output* types in `acme::dns` (the parsed DNS answer, the propagation
  timeout, the credential, the HTTP request/response) are `#[non_exhaustive]`
  from the start. `AcmeConfig`, `AcmeDnsConfig`, `AcmeRenewalTask` and
  `DnsChallenge` are not:
  callers build them by struct literal and they have no constructor, so sealing
  them would make them unusable — the same reasoning that left `ServerConfig`
  open. See the [migration guide](docs/migrations/next.md).

  `autumn-cli`'s `tls` feature now also enables `autumn-web/acme`: `autumn
  doctor` grades the DNS-01 credential with the runtime's own
  `validate_credential` and probes `_acme-challenge` visibility with the
  runtime's own resolver, so the check and the server cannot disagree about what
  a usable configuration is. Additive — the resolved feature set is a superset
  of what `tls` already selected.
- **`autumn plugin remove` and scaffold-time `--with` plugin flags (#1631):**
  `autumn plugin add` (#1606) made installing a plugin one command, but the
  lifecycle only ran one way — a repo-wide grep found no uninstall story at
  all, and `autumn new` had no plugin flag, so every plugin was a retrofit even
  when the user knew at day zero they wanted one. Because the install is
  machine-applied, the removal can be machine-reversed:

  ```bash
  autumn new my-app --with autumn-admin-plugin   # wired on day zero
  autumn plugin remove autumn-admin-plugin       # both wires back out
  ```

  `plugin remove` deletes the `[dependencies]` line and excises the
  `.plugin(...)` / `.with_blob_store(...)` call — marker comment included — as
  a balanced-paren span, so a mount configured across several lines comes out
  whole. An app installed with `plugin add` is byte-identical afterwards and
  passes `cargo check` on the first try.

  It declines rather than guesses, in the three places a guess would leave an
  app that does not compile: a mount it cannot read as a single builder call
  (a plugin built into a variable, or a one-line chain) changes **nothing** and
  prints the lines to delete; a dependency still named anywhere under `src/`,
  `tests/`, or `benches/` is kept, with the file that kept it named; a
  community mount is never deleted, because `add` never wrote one. Partially
  wired plugins — the shape a manual README install leaves — are unwired as far
  as they go, with the missing half reported, and removing a plugin that is not
  installed is an idempotent no-op.

  **The database is never touched by default.** A plugin that declares
  migrations or owns tables gets them listed, with a statement that they are
  still there; `--drop-data` reverts them, printing the exact statements and
  asking for confirmation first (`--yes` for CI; a non-interactive stdin
  without it is a refusal, not an assumed yes). `--dry-run` writes nothing and
  distinguishes its answer in the exit code: `3` when a real run would change
  something — pending *database* work included, so an already-unwired plugin
  whose tables remain still answers `3` — and `0` when there is nothing to do.
  `--drop-data` is refused outright when the mount cannot be unwired: dropping
  what a still-mounted plugin owns would break the running app, so nothing is
  asked and nothing is changed.

  `autumn new --with <plugin>` is repeatable and resolves every name — curated
  catalog first, crates.io fallback — and version-checks it **before the
  scaffold writes a byte**, so a typo never leaves a half-built project behind.
  With `--starter`, whose own `autumn-web` pin is not knowable until the starter
  is fetched, names are still resolved up front and the version answer arrives
  afterwards as "the app was created, the plugin was not wired" (exit 2) rather
  than as a failed `autumn new`.
  `autumn doctor` gains a `plugin_residue` check under the existing
  `--json`/`--strict` contract: a dependency with no mount warns, a mount with
  no dependency fails (it does not compile), and migrations left applied by a
  plugin that is gone warn. A CI gate round-trips every first-party plugin
  through `new --with` → `cargo check` → `plugin remove` → `cargo check` →
  zero doctor residue.

- **Dependency-advisory gate, on by default, for scaffolded apps and Autumn's
  own releases (#1600):** the CI workflow `autumn new` generates relegated
  vulnerability auditing to a comment ("Optional extensions… Audit: `cargo
  install cargo-audit`"), which almost nobody enabled, so apps shipped with
  known-vulnerable transitive dependencies and found out from a pentest rather
  than from CI. A generated app now audits its whole dependency tree on every
  push and pull request, and a known RustSec advisory fails the build:

  - `.github/workflows/ci.yml` installs a pinned cargo-deny and runs `cargo deny
    check advisories`, reading the new **`deny.toml`** the scaffold writes at
    the project root. Waive an advisory by adding an `ignore` entry there with
    its id, a `reason`, and a review-by date — the gate stays on and lets
    exactly that one id through; an unwaived advisory still fails.
  - Day-one CI is green for every flavor: the scaffold ships documented
    waivers for the advisories its own tree cannot avoid — RUSTSEC-2023-0071
    (`rsa` via the unconditional `jsonwebtoken` dependency, no patched release
    exists) everywhere, plus RUSTSEC-2024-0384 (`instant`, via the
    embedded-Postgres build stack) for `--bundled-pg` apps and only those.
    Autumn's own CI re-audits autumn-web's tree — with every feature a scaffold
    flavor can enable — against that exact policy on every run, so the waiver
    set cannot quietly stop covering what the scaffold ships.
  - An app upgraded from an older release receives the workflow but not the
    policy (`deny.toml` is the app's file, never reconciled): the audit step
    detects that and says which file to add, rather than auditing under
    cargo-deny's unwaived default. See `docs/migrations/next.md`.
  - When the advisory database is unreachable the gate **fails closed**: the
    fetch is its own step, retried three times with backoff, and the audit then
    runs `--offline` against it — no hang, no silent skip, and a failure in the
    audit step always names a real advisory.
  - Autumn's own release path is gated the same way: `scripts/check-advisories.sh`
    runs in PR CI *and* in the Publish Gate (a `prepare-release` dependency), so
    a release with an unwaived advisory in its tree cannot be tagged. Its
    `--self-test` proves the gate can still go red by auditing an injected
    known-vulnerable dependency (`time 0.1.45`, RUSTSEC-2020-0071) and requiring
    rejection, then acceptance once that id is waived.
  - Docs: [supply-chain guide](docs/guide/supply-chain.md) covers what the gate
    checks, how to read a failure, and how to waive an advisory.

- **A published Windows support policy, enforced by a `windows-latest` journey
  gate (#1616):** the PRD promised "developers build on macOS and Windows", but
  nothing said what a Windows developer could actually expect, and the native
  journey degraded silently. `autumn dev` stopped the app with
  `TerminateProcess`, which skips `on_shutdown` hooks — so a managed Postgres
  cluster was orphaned on every hot reload — and `autumn deploy up` staged
  secrets without the `0600` its Unix path applies.

  There are now two tiers, published in
  [Platform support](docs/guide/platform-support.md) and in the README. **Tier 1
  works natively on Windows**: `new`, `doctor`, `setup`, `dev`, `test`,
  foreground `serve`, managed Postgres, and the local-only `deploy check` /
  `deploy plan`. **Tier 2 is supported via WSL2**: the `serve --daemon`
  lifecycle, the `deploy` actions that reach a host over SSH (`up`, `rollback`,
  `status`, `maintenance`), and the bash contributor gate scripts. Tier 2
  commands now **fail fast** on native Windows with an error
  naming the tier, the reason, and the policy — instead of half-working. (The
  two script-shaped Tier 2 entries — `scripts/*.sh` and the browser
  `SystemTest` suites — have no autumn entry point to refuse from, so for those
  the tier is documentation.)

  The `dev` teardown is fixed rather than documented away. The runtime accepts
  a cooperative shutdown request through `AUTUMN_SHUTDOWN_SIGNAL_FILE` (opt-in;
  unset changes nothing) and drains through the same graceful path a signal
  takes on Unix, so shutdown hooks run and the managed cluster stops cleanly. If
  an app misses that budget, `autumn dev` force-stops it **and says the hooks may
  not have run** — degraded, never silent. The budget is the app's own
  (`prestop_grace_secs + shutdown_timeout_secs`, resolved through the same
  profile-aware reader `autumn serve stop` uses) plus headroom for the hooks
  that run after the drain, so an app that legitimately takes 35 seconds to
  shut down is not cut off early.

  `autumn doctor` gains a `platform_support` check reporting the platform's tier
  and the Windows prerequisites (the vcpkg/OpenSSL requirement for
  `generate auth --passkeys`). The tier table lives in one place
  (`autumn-cli/src/platform.rs`); the doctor check and every fail-fast message
  read from it, and a parity test fails the build when the guide's two tier
  tables are not exactly the table's two tiers — moving one row between tiers in
  either file turns it red. A `windows-tier1` CI job walks the whole Tier 1
  journey — scaffold, `doctor`, `setup`, dev-loop edit/rebuild/reload, managed
  Postgres boot and clean shutdown — on every pull request into `trunk-dev`. On
  its first run the gate immediately earned its keep, surfacing a Windows-only
  link failure (`LNK4319`, the PDB public-symbol limit) that a debug build of a
  `--bundled-pg` scaffold hits and that the pre-existing `cargo test
  --workspace` Windows leg could never see, because it never builds a
  scaffolded app. The workaround is documented in the platform-support guide;
  the product-level fix is tracked separately.
- **Security posture diffs gate pull requests, and the shipped manifest is
  signed (#1624):** #1604's manifest proves what an app's security surface
  *is*; a manifest nobody diffs is a report, not a control. `autumn routes
  posture` closes that: `diff` compares two manifests and classifies every
  change as widening, neutral or narrowing; `digest` prints the posture digest
  a release records; `verify` proves at deploy time that a shipped manifest is
  the posture CI acknowledged **and** was signed by CI (`gh attestation
  verify`, reusing #1615's keyless pipeline rather than introducing a second
  signing story).

  Only widening blocks — a new public route, a guard removed, a classification
  downgraded — and the rules follow the semantics the framework actually
  implements: roles are OR-ed, so *adding* one widens; scopes are AND-ed, so
  *removing* one widens. Routes are keyed on their *shape* — capture names
  erased, capture kinds kept — and handler names and source locations are
  excluded from both the comparison and the digest, so a refactor produces no
  finding at all. A change with no posture effect posts nothing.

  Because a route is not a URL, the diff follows the router's own precedence:
  deleting a gated `/users/me` while a public `/users/{id}` remains is a
  widening (that URL falls through), and adding a route that takes a stricter
  route's URLs over is one too. Configured `security.csrf.exempt_paths` are
  compared as posture in their own right, since the per-route rows cannot show
  them.

  A widening is unblocked by one comment on the pull request:

  ```
  /ack-posture 4f8a1c0d9e2b7a35  intentional: public status page for launch week
  ```

  The digest binds the acknowledgment to that exact set of widenings, so
  unrelated pushes keep it valid while a *new* widening re-blocks. That comment
  is also the documented escape hatch for a false positive: there is no flag
  that disables the gate or hides the diff, so a wrongly blocked pull request
  is always unblockable by the team alone, in public, with a reason.

  Both digests use an escaped canonical encoding, so a crafted route path
  cannot make one finding hash like a set of ordinary ones; every dimension is
  compared from *both* sides, so a fact that disappears from the manifest (for
  example every POST leaving the CSRF dimension when `security.csrf.safe_methods`
  grows) is still reported as the loss it is; and the scaffolded workflow
  resolves each commenter's real repository permission rather than trusting
  `author_association`, refuses to run on a pull request that edits the gate
  itself, and fails rather than bootstrapping when a committed baseline
  disappears from the base branch.

  The scaffolded workflow is two jobs: one compiles the pull request and emits
  its manifest with no write permission and no verdict, and one that never
  compiles anything downloads that manifest, diffs it, and decides — so a build
  script in the diff cannot replace the binary that computes the verdict.

  `autumn new` scaffolds `.github/workflows/posture-gate.yml` by default;
  existing apps adopt it with `autumn upgrade --apply`. The scaffolded deploy
  workflows attest `security-posture.json` with
  `actions/attest-build-provenance`, and autumn's own `examples/hello` runs the
  gate in the publish gate (`scripts/check-posture-gate.sh`). See
  `docs/guide/posture-gate.md`.

  `posture-gate.yml` and `ci.yml` still install the `autumn` CLI pinned to
  the app's own `autumn-web` version by default (#2495): pinning alone left
  every newly scaffolded gate red between a subcommand landing and the next
  release, and stuck red afterwards until someone re-ran `autumn upgrade
  --apply` — but always installing the latest published release instead
  would trade that for a worse problem, running these gates under a CLI
  this project's own compatibility check (`autumn doctor`) calls
  incompatible the moment a minor release ships, with the gap only growing
  as later, unrelated releases ship. So only `posture-gate.yml`'s verdict
  job — which never compiles the pull request, only reads JSON — falls
  back, for that run alone, when the pinned CLI lacks `routes posture`: it
  probes forward through the next few releases and installs the first one
  that has it, landing on a specific, bounded release rather than a moving
  "latest" that keeps drifting from the app's own pin. The fallback does
  not resolve itself — the workflow still installs the app's pinned
  `autumn-web` version first on every run — so it keeps firing until the
  app raises that pin (and reruns `autumn upgrade --apply`) to a release
  that already has the command. `ci.yml`'s `a11y verify` and `routes audit`
  steps (like
  `posture-gate.yml`'s own `manifest` job) compile and introspect the pull
  request's own code, so they never fall back — they now probe for their
  subcommand the same way `posture-gate.yml`'s verdict job already did, and
  fail with an actionable message naming the pin to raise, rather than a
  raw unknown-subcommand error. Scaffolded workflow YAML only; no new or
  changed CLI surface for agents to reach for. [no-plugin]

- **Build-time authority envelope for agent-operable handlers (#1691):** an
  endpoint exposed as an MCP tool is an action an autonomous agent can take
  with no human in the loop, and nothing said what that action was *allowed*
  to do. `#[api_doc(mcp)]` published a description; the blast radius — which
  models the handler writes, whether it can erase a table, whether it can leave
  the tenant it was invoked for, which hosts it reaches, which jobs it starts,
  how hard the whole thing is to undo — lived in the reviewer's head and
  drifted the first time someone added a line to the body. Autumn now makes it
  a declared value the compiler checks. `authority_grant!` declares a named
  `const Grant`, and `#[agent_operable(grant = RefundDrafter)]` walks the
  handler body, derives the effect set it can prove, and emits one const-eval
  coverage assertion per proved effect, respanned onto the offending call:

  ```rust
  use autumn_web::prelude::*;

  authority_grant! {
      pub RefundDrafter {
          writes: [Refund],
          tenant_scope: scoped,
          outbound: ["https://api.stripe.com/v1/refunds"],
          jobs: [NotifyFinanceJob],
          rate: "10/min",
          spend: "500.00 USD",
          reversibility: compensable,
      }
  }

  #[post("/api/refunds")]
  #[api_doc(mcp, summary = "Draft a refund")]
  #[agent_operable(grant = RefundDrafter)]
  pub async fn draft_refund(/* … */) -> AutumnResult<Json<Refund>> { /* … */ }
  ```

  Adding `payouts.create(&p).await?` to that body fails `cargo build` at the
  write, on every branch, whether or not a test exercises it — and because the
  check is const-evaluated against the linked `Grant` rather than against
  tokens, it holds when the grant is declared in another crate. Six effect
  kinds are recognised: bounded writes, unbounded writes (`delete_all`,
  `truncate`, an unfiltered `diesel::update` — never implied by `writes`,
  because one row and all of them are different authorities), cross-tenant
  access, outbound HTTP, webhook topics and job enqueues. A raw diesel
  `SELECT`/`UPDATE`/`DELETE` run on the request's connection carries none of
  the tenant predicate the repository codegen applies, so it is recorded as a
  cross-tenant effect (`raw_query:<table>`) and refused under the default
  `tenant_scope: scoped` — route it through a repository, declare the statement
  `#[agent_effect(scoped, reason = "…")]`, or declare `tenant_scope:
  cross_tenant`; an `INSERT` has no `WHERE` to scope and is exempt. Declared
  `reversibility` has a floor the proved effects impose: a job, a webhook, an
  outbound call or an unbounded write cannot be undone by writing the previous
  rows back, so none of them may be declared `reversible`. (A cross-tenant read
  can be, so it carries no floor of its own; a cross-tenant write still carries
  its write effect's.) The analysis is fail-closed where it has no chokepoint
  to rely on: `job::enqueue` reaches a global client, `Client::new()` is
  constructible from nothing, and a webhook fans out to subscriber-supplied
  URLs, so those verbs are effects wherever they appear and their subject must
  be a literal. Anything opaque — a helper handed a tracked handle, a
  `format!`-built URL, a non-literal job name or webhook topic, a
  `dyn`/`impl Trait` handle, a `tokio::spawn` that detaches the effect from the
  request it is audited under — is a diagnostic naming the call site and the
  annotation that discharges it, never a silent zero. The one escape hatch,
  `#[agent_effect(writes(Refund), reason = "…")]` on a statement (`writes`,
  `unbounded_writes`, `cross_tenant`, `outbound`, `webhooks`, `jobs`, `scoped`,
  `none`, and a mandatory non-blank `reason`), declares what the analysis
  cannot read — and declared effects are checked against the grant exactly like
  proved ones, because the hatch declares, it never grants.

  `autumn agents manifest` builds the app and reads back the diffable record:
  one row per action with its envelope, its proved effects and the grant
  entries nothing exercises, every declared grant including unused ones, and —
  the completeness half — every MCP-exposed tool with *no* envelope at all.
  `--check` fails on drift and on any ungoverned **mutating** tool unless
  `--allow-ungoverned` is passed, which is the one gap the compiler cannot
  catch: a tool with no grant has no assertion to fail. Every MCP `tools/call`
  now writes two audit events with zero per-handler wiring —
  `agent.tool.<name>.attempt` before dispatch and `agent.tool.<name>` after —
  sharing a correlation id and carrying a `phase` (`attempt` / `outcome` /
  `refused`), the transport, the grant, the compile-known reversibility, the
  proved effect set and the argument *names* — only the keys the tool declares,
  any others counted as `+N unknown`, never their values — with the outcome
  additionally carrying the HTTP status and the pipeline's own `x-request-id`.
  An ungoverned tool is audited too, with `reversibility = "unknown"`. Every
  write is bounded by a 2-second timeout; if the attempt record cannot be
  written or times out and the action is not `reversible`, the tool fails
  closed, the handler never runs, and a best-effort
  `agent.tool.<name>.refused` (status Failure, carrying a `refused_reason`)
  records that it was turned away. `destructiveHint` now takes the declared
  reversibility as an input the HTTP verb alone could not supply — raising a
  `POST`/`PATCH` the verb says nothing about, while never clearing the warning
  a `DELETE` already carries — and handlers can read the invocation through
  `Extension<AgentInvocation>`. `--check` additionally fails when a binary has
  no audit sink *and* can still take an agent-reachable action nothing can undo
  (`--allow-unaudited` accepts it), and on a route naming an authority nothing
  registered.

  The first slice is deliberately narrow and says so in the manifest itself:
  `rate` and `spend` are validated for grammar and recorded but **not enforced
  at runtime** — there is no metering in this slice; generated
  `#[repository(api, mcp)]` CRUD tools have no annotation site and therefore
  surface as ungoverned rather than being gated; and `dependent(...)` cascades
  are not folded into write sets, so `writes: [Post]` does not imply the
  comments a delete takes with it. Each of those is named in the document's
  `excluded` list with its caveat, rather than left to the guide. See
  `docs/guide/agent-authority.md`.
- **`autumn_web::contains_letter_or_number(&str) -> bool` (#2424):** the
  predicate to use when rejecting user input that will be slugified. `slugify`
  never returns an empty string — input with nothing to slugify gets a stable
  hash fallback token — so `slugify(input).is_empty()` is always `false` and
  can only ever be dead code. This asks the question that check was reaching
  for: does the input hold at least one letter or number, in any script?
  (Precisely, any `char::is_alphanumeric`, so `"½"` and `"Ⅻ"` count too.) It is
  deliberately broader than "`slugify` produced a real slug", so `"日本語"`
  passes — real text, hashed URL segment — while `"***"` and `"🎉🔥💯"` do
  not. It is a content check, not a spoofing defence: a handful of characters
  are letters by Unicode yet render blank (the Hangul fillers), and filtering
  those is the application's job. `slugify`'s own behavior is unchanged; its
  doc comment now points here.
- **A versioned stability contract for the plugin API (#1601):** Autumn shipped a
  real plugin system — the `Plugin` trait, a conformance harness, a flagship
  first-party plugin, authoring docs — and no compatibility contract to go with
  it. Nothing said which plugin-facing APIs were stable, a plugin could not
  state which `autumn-web` versions it supported, and no CI gate was specific to
  the plugin surface, so every release was a potential silent break for anyone
  building on it. Non-breaking: every new API is additive, and a plugin that
  declares nothing behaves exactly as it does today.

  Plugin-facing APIs are now declared `stable` or `experimental` in
  `autumn_web::plugin_contract::PLUGIN_SURFACES`, with the SemVer promise each
  tier carries written down in [`STABILITY.md`](STABILITY.md#the-plugin-api-surface-issue-1601)
  and the table rendered in [the plugin guide](docs/plugins.md#the-plugin-api-contract).

  A plugin declares the framework range it supports, and an excluded pairing
  fails at registration naming both versions and both remedies:

  ```rust
  fn contract(&self) -> Option<PluginContract> {
      Some(PluginContract::new(env!("CARGO_PKG_NAME")).autumn_web("0.7"))
  }
  ```

  The framework side is gated by compilation: `autumn-plugin-reference` is a
  pinned reference plugin that calls every declared stable surface, built by the
  new `plugin-contract` CI job on every change — so removing, renaming, or
  re-signaturing one is a red check on the PR that causes it. A stable entry
  with no call site in that crate fails the same job, so the registry cannot
  promise what nothing compiles.

  `autumn plugin-check` gains two checks, `plugin-contract` and
  `experimental-surface`, reading the contract the built binary dumps; both skip
  on a binary that predates the dump, and `--deny-experimental` fails closed
  rather than becoming a silent no-op. `autumn generate plugin` now scaffolds
  `Plugin::contract`. The migration-guide template gains a **Plugin authors**
  section, and `scripts/check-plugin-surface.sh` fails a change to the declared
  surface that does not fill it in. <!-- migration-guide-gate: the additive half
  of #1601; its one break is declared separately under Changed -->

- **Pin a worker tier to queues from the command line, and let `doctor` prove
  fleet-wide queue coverage (#1623):** per-queue `reserved`/`concurrency` pools
  and `jobs.pin` already existed, but pinning could only be spelled in
  `autumn.toml` or `AUTUMN_JOBS__PIN`, and the `[jobs.fleet]` topology that lets
  `autumn doctor` hard-fail on an uncovered queue was absent from the config
  schema — declaring it warned as an unknown key (and failed boot under
  `server.strict_config_enforce_all`) — and went unmentioned in the jobs guide.

  `autumn serve` now takes `--pin`, repeatable and comma-separated, forwarded to
  the app as `AUTUMN_JOBS__PIN` and restored by `autumn serve restart` so a bare
  restart can't silently turn a pinned worker tier into an unpinned one:

  ```bash
  autumn serve --role worker --pin critical
  ```

  `[jobs.fleet]` (`tiers`, `manifest`, `declared_queues`) is now a first-class,
  validated config section. It is purely declarative — no process acts on it at
  runtime — and an app that declares nothing keeps today's behavior exactly. See
  [Background Jobs](docs/guide/jobs.md#per-queue-worker-pools-caps-and-pinning).

  Also adds the end-to-end coverage the acceptance criteria asked for: a pinned
  worker never claims an out-of-subset queue on **both** the Postgres and Redis
  backends, a per-queue `concurrency` cap bounds in-flight jobs below the worker
  count, and p95 enqueue-to-start latency on a queue with dedicated capacity
  stays within 2x its unloaded baseline while another queue floods.
- **A proven capacity contract that travels with the build (#1733):** autumn
  already shed load once too many requests were in flight, but the ceiling it
  enforced was a hand-tuned guess in `autumn.toml`, and nothing told an operator
  what a given binary could actually sustain before they deployed it. Capacity
  planning was a spreadsheet and a hope. Now it is a lockfile:

  ```sh
  autumn calibrate          # measure → capacity.lock
  autumn calibrate --check  # gate a rebuild against the committed contract
  ```

  `autumn calibrate` builds the app in release mode, reads its route graph back
  through the same `AUTUMN_DUMP_ROUTES` pipeline `autumn routes` uses, boots it
  with admission control switched off (so the run measures the app, not a
  ceiling it was already carrying), and walks a seeded concurrency ladder. The
  **saturation knee** — the last rung where more concurrency still bought
  materially more throughput — becomes the recorded envelope:

  ```toml
  [envelope]
  sustained_rps = 4210.5
  p99_latency_ms = 18.42
  saturation_concurrency = 64
  admission_limit = 128

  [[routes]]
  method = "GET"
  path = "/posts"
  shape = "db-bound"
  pools = ["db"]
  ```

  Each route's `shape` is derived **statically**, at macro-expansion time, from
  the extractors its handler declares — a provable subset, so a route reading
  `compute-bound` means "no pool proven", never "no pool touched". Committing
  the contract makes `autumn calibrate --check` a CI gate that fails with a
  human-readable diff when a rebuild leaves the envelope, and pointing
  `[server] capacity_contract` at it makes the binary admit against its own
  proven edge instead of a guess:

  ```toml
  [server]
  capacity_contract = "capacity.lock"
  ```

  The contract also records the workload that produced it — profile, Cargo
  features, seed, ladder, rung duration and repeat count — and `--check`
  replays all of it rather than its own defaults, since an envelope only means
  something next to the experiment behind it. Each rung is measured three times
  and the median kept: a single sample per rung spread by up to 20% across
  no-op rebuilds of an identical build on a shared runner, which is wider than
  the regression tolerance itself. Setting
  `[server] capacity_contract` also makes `autumn deploy` ship the contract
  alongside the manifest that names it.

  Both the gate and the runtime refuse to compare envelopes across host classes,
  and every failure along the contract path (missing file, malformed document,
  a contract from another machine, a recorded limit of `0`) degrades to
  *unlimited* with a warning rather than to a ceiling — failing closed would
  mean a typo'd path sheds every request on the way up. An explicit
  `server.max_concurrent_requests` always wins, including an explicit `0`. See
  [docs/guide/capacity-contracts.md](docs/guide/capacity-contracts.md).
- **Authored, seeded fault scenarios you can commit as regression tests
  (#1680):** the simulation harness could already inject faults, but only
  *probabilistically* — `Chaos::db_transient_errors(0.05)` says "5% of
  connection checkouts fail" and lets the seed pick which. That is the right
  tool for a sweep hunting rare interleavings and the wrong one for proving a
  fix, because the sentence a post-mortem produces is "the third connection
  checkout failed while the second `send_invoice` execution was retrying", and
  no rate reproduces that on purpose. A new `autumn_web::sim::FaultPlan`
  authors that scenario by ordinal, attaches to a `TestApp` with no application
  code changes, and hands back a serializable record of what happened:

  ```rust
  use autumn_web::sim::{FaultPlan, Sim};

  #[sim_test]
  async fn the_third_checkout_and_the_second_invoice_fail(mut sim: Sim) {
      let plan = FaultPlan::from_seed(sim.seed)
          .fail_db_checkout(3)           // 3rd checkout on any pool (1-based)
          .fail_job("send_invoice", 2);  // 2nd execution of that job by name

      sim.build(
          TestApp::new()
              .routes(routes![checkout])
              .plugin(InvoiceJobPlugin)
              .with_fault_plan(plan),
      );

      for _ in 0..5 {
          sim.client().post("/checkout").send().await;
      }
      sim.run_to_idle().await;

      let outcome = sim.client().fault_outcome().await;
      assert_eq!(outcome.fired.len(), 2);
      assert_eq!(outcome.server_errors[0].status, 503);
      assert_eq!(outcome.final_state.db_checkouts, 5);

      // Canonical JSON, byte-identical on every replay of this seed.
      assert_eq!(outcome.to_json_string(), include_str!("fixtures/invoices.json").trim_end());
  }
  ```

  Two effect classes fire deterministically through the existing
  `interceptor.rs` seams: database connection checkout (`fail_db_checkout`,
  `fail_db_checkout_on("replica", 2)`) and job execution
  (`fail_job_execution`, `fail_job("send_invoice", 2)`), each targetable by a
  1-based ordinal on a global or per-target counter. `random_db_checkout_faults(2, 1..=8)`
  derives ordinals from the plan's seed and resolves them into explicit entries
  at builder-call time, so `plan.planned()` always describes the whole schedule
  and an explicit-only plan draws no entropy at all. `only_between(from, to)`
  confines faults to a half-open window of elapsed time measured on the app's
  **injected** clock, so `Sim::advance` moves it and no wall-clock read leaks
  in; an effect outside the window still consumes its ordinal and is recorded
  as `suppressed` rather than silently vanishing.

  `TestClient::fault_outcome().await` returns a `FaultOutcome` carrying the
  seed, `fired` (effect, target, global and per-target ordinal, the injected
  clock's `at` and `elapsed_ms`), `suppressed`, `unfired`, `server_errors`
  projected from `reporting.rs`, and `final_state` seam totals. It is
  `Serialize + Deserialize + PartialEq`, its `to_json_string()` is canonical
  (declaration-order fields, no maps, no floats) and `fingerprint()` is an
  FNV-1a 64 over it — so a scenario replayed 100× from one seed produces a
  byte-identical record 100/100 times, which is exactly what the committed
  determinism test asserts. The `async` on `fault_outcome` is load-bearing: it
  settles autumn's detached error-report tasks with bounded cooperative yields
  before snapshotting, without advancing the virtual clock.

  A plan **composes** rather than replaces — it chains behind your own
  `with_job_interceptor` / `with_db_interceptor`, the always-on job recorder,
  transactional-DB isolation and `Sim::chaos`, with the injected failure
  innermost so a user interceptor observes it exactly like a real handler
  error. Attaching a plan also defaults the app's entropy to `SeededEntropy`
  from the plan's seed (an explicit `with_entropy` still wins) and asserts the
  settings replay depends on — one job worker, reporting at
  `sample_rate = 1.0`, and failure capture off — instead of letting a second
  worker, a sampled-out 5xx, or a capsule write that reporting awaits before
  any reporter runs quietly break reproducibility.

  Scope is deliberately narrow: test-only (there is no production fault
  injection), DB checkout and job execution only (use `Chaos::smtp_faults` for
  SMTP), and `TestClient::perform_enqueued_jobs` bypasses the
  `intercept_execute` seam so job faults require `Sim::run_to_idle`.
  `autumn/tests/integration/sim_fault_plan.rs` is the worked proof: a
  `charge_card` job whose scenario fails at `max_attempts = 1` and passes at
  `max_attempts = 3`, the same before/after shape as the retry-storm example.
  See
  [Simulation Testing → Authored fault scenarios](docs/guide/simulation-testing.md).
- **A markdown link gate for the docs corpus [no-plugin]:** `check-docs.sh`
  gated rustdoc intra-doc links and `check-plugin-freshness.sh` gated the
  `docs/guide/*.md` paths named from `skills/` and `agents/`, but nothing
  checked the 383-file markdown corpus itself — so a guide could link to a
  page that was renamed, never written, or lives one directory up, and
  nothing noticed. `scripts/check-docs-links.sh` resolves every relative
  link and heading anchor in tracked markdown and now runs in CI's docs-only
  job. Its baseline found **19 broken links across 11 pages**, all fixed
  here: five rustdoc paths pasted into `aggregates.md` as markdown targets
  (they render as links to a directory that does not exist), three
  `docs/design/` links off by one directory level, `authorization.md` and
  `generators.md` pointing into `docs/api/` and `docs/reference/` trees that
  have never existed, `tauri.md` pointing at a `managed-pg.md` that is
  really `daemon.md`, two guides promising a `custom_config_loader` example
  that is not in the workspace, and four heading anchors that no longer
  match their headings. External links are deliberately out of scope
  (network-flaky, and not fixable in this repo).
- **A CLI drift gate for the docs corpus [no-plugin]:** the link gate stops a
  reader being sent to a page that does not exist; nothing stopped them being
  handed a *command* that does not exist — the other thing they copy off a
  page. `autumn-cli` carries 174 command paths and the reader-facing docs name
  them 2,400+ times, so a renamed or never-shipped subcommand leaves behind a
  line that looks exactly like a working one.
  `scripts/check-docs-cli.sh` resolves every `autumn …` invocation in fenced
  shell blocks and inline code spans against the command tree parsed from the
  clap derive input, and now runs in CI's docs-only job. Its baseline found
  **11 defects across 6 guide pages**, all closed here: eight occurrences of
  `autumn migrate run` — a command `MigrateCommands` has never had (`status`,
  `check`, `down`, `baseline`; the run action is the bare `autumn migrate`),
  one of them in a fenced `shell` block in `cloud-native.md` under "run the
  migration before deploying new workers", where clap answers `unrecognized
  subcommand 'run'` to a reader mid-production-upgrade — plus
  `autumn system-test check` sitting inside a runnable block in
  `system-tests.md` as a planned command, now moved out of the block. The
  truth set is parsed from `autumn-cli/src/**/*.rs` rather than a checked-in
  snapshot, so a rename moves the gate with it in the same commit; a page that
  deliberately names a command that does not exist (`autumn generate island`,
  `autumn generate seed`) waives it inline with a stated reason. Flags are
  deliberately out of scope, as are the changelog and `docs/plans/`, which
  name commands that were true once or are not true yet.

- **One retention policy for every table Autumn creates (#1605):** every
  deployed Autumn app accumulated framework-owned data forever by default —
  job history, tracking records, idempotency responses, experiment
  assignments, webhook replay markers, sessions, audit archives. Retention
  existed only piecemeal (`jobs.tracking.ttl_secs`, `idempotency.ttl_secs`),
  so "keep operational data 90 days" meant discovering each subsystem's
  private knob, finding there often wasn't one, and hand-writing cron jobs
  against undocumented tables. A new `[retention]` section in `autumn.toml`
  declares a window per dataset, and Autumn enforces it on a recurring,
  fleet-coordinated in-process sweep — no external cron:

  ```toml
  [retention]
  job_history            = "90d"
  job_tracking           = "7d"
  experiment_assignments = "365d"
  audit_archives         = "400d"
  ```

  Postgres-backed datasets are swept in bounded batches against the database's
  own clock; TTL-native stores (idempotency, webhook replay, sessions) have
  their record TTL *capped* at the window; the JSONL audit archive is rewritten
  atomically without the stale entries, keeping any line it cannot parse. The
  pre-existing `jobs.tracking.ttl_secs` / `idempotency.ttl_secs` knobs keep
  working unchanged: the documented rule is that the **shorter** bound wins, so
  adding `[retention]` can never cause data to be kept longer than it is today.
  Leaving a dataset unset registers no sweep task at all.

  Data under a GDPR legal hold (`ModelRegistration::retain`) is never removed —
  the hold vetoes the whole dataset rather than filtering rows — and every real
  sweep writes an audit record carrying the dataset, the cutoff timestamp and
  the rows removed, including one that removed nothing and one a hold blocked.
  `autumn db retention [--dry-run|--purge] [--dataset X] [--json]` reports the
  effective window per dataset, which setting produced it, how it is enforced,
  and how many rows are eligible right now; it runs inside your app binary so
  the report and the enforcement come from one code path. A
  `retention.webhook_replay` window shorter than a configured endpoint's
  `replay_window_secs` fails boot rather than silently weakening replay
  protection. See
  [Data Retention for Framework-Owned Data](docs/guide/data-retention.md).

  **Breaking:** `autumn_web::audit::AuditEvent` gains a `metadata:
  BTreeMap<String, String>` field so a sweep record can carry its dataset,
  cutoff and row count. Only code that constructs or destructures `AuditEvent`
  *by struct literal* is affected — `AuditEvent::new(...)` is unchanged, the
  field is `#[serde(default)]`, and archives written before this release still
  deserialize. `AuditSink::purge_before` is a *provided* method, so existing
  sinks keep compiling untouched. See the
  [migration guide](docs/migrations/next.md).

- **SSG records each route's intended `Content-Type` at generation time
  (#1832):** the static-first serve path used to reverse-engineer every cached
  response's MIME type at *request* time, from the route slug plus the served
  file name. Because `url_to_file_path` stores every non-root route as
  `<route>/index.html`, both clues lie: `/sitemap.xml` lands at
  `sitemap.xml/index.html`, whose file name says HTML. That guess needed three
  consecutive corrections during review of #1819 — pre-compressed fonts,
  generated `.txt`/`.xml` routes, and HTML pages whose slug merely contains a
  dot (`/posts/release.v1`, `/users/alice@example.com`) — each round fixing a
  case the previous one broke.

  `render_static_routes` now records the `Content-Type` the handler declared on
  each rendered page into a new optional `content_type` field on
  `ManifestEntry`, and the static-first middleware serves that value directly
  via the new `StaticFileLayer::resolve_entry`. The type is determined once,
  where it is actually known, and never inferred again — which makes all three
  edge cases impossible by construction and lets a route be served as a type no
  file extension maps to (`application/rss+xml` from `/feed`, `text/calendar`
  from `/calendar`), something the extension heuristic could never produce.

  Existing `dist/` directories keep working untouched. `content_type` is
  `#[serde(default)]`, so a manifest built before this change (or written by
  hand) deserializes with the field absent, and `static_gen::resolved_content_type`
  applies the pre-#1832 derivation byte-for-byte: recognized route extension,
  then served file name, then `application/octet-stream`. Nothing is recorded
  for a handler that declares no `Content-Type` either — a build-time guess
  would only bake in the heuristic this change removes. That function also
  rejects a recorded value that is not a legal header (a CR/LF injection
  attempt from a tampered manifest) and falls back instead, returning a
  `HeaderValue` so the request path cannot panic on a bad manifest.

  ISR does not rewrite the manifest (it is immutable behind an `Arc`, with file
  mtime driving staleness), so the header served for a route is fixed for the
  process lifetime while the body on disk is not. A regeneration whose handler
  declares a *different* type — or stops declaring one — is therefore refused
  rather than written: the previous file stays, still matching its recorded
  type, so the route degrades to stale-but-correct instead of serving fresh
  bytes under a header that mislabels them. The refusal is logged; `autumn
  build` re-records the type.

  Only a type the handler *deliberately* declared is recorded. axum's blanket
  `IntoResponse` impls always attach one — `text/plain; charset=utf-8` for
  `String`, `application/octet-stream` for `Vec<u8>` — purely from the return
  type, so recording those over a route that names its own extension would have
  served `#[static_get("/theme.css")] async fn theme() -> String` as plain text
  and let `X-Content-Type-Options: nosniff` drop the stylesheet outright. When a
  declared type is one of those two generic defaults and the route's final
  segment carries a recognized asset extension that disagrees, nothing is
  recorded and the derivation runs as before. An explicit declaration still
  wins, even against the slug (`/notes.txt` declaring `application/json`).

  One neighbouring fix fell out of the same work: a manifest that is *present
  but unparseable* now logs a warning instead of disabling static serving with
  no trace at all (absent stays quiet — that is just an app with no static
  build). `ManifestEntry::revalidate` also gained an explicit `#[serde(default)]`
  to state that a hand-written entry may omit the key, though it changes
  nothing on its own — serde's derive already maps a missing `Option` field to
  `None`, so the shortest documented entry parsed before this release too.

  **Breaking:** `ManifestEntry` and `StaticManifest` are now `#[non_exhaustive]`
  and `ManifestEntry` gained a `content_type` field, so struct literals — and
  exhaustive destructuring patterns like
  `let ManifestEntry { file, revalidate } = entry;` — must
  become `ManifestEntry::new(file).with_revalidate(..).with_content_type(..)`
  and `StaticManifest::new(routes)`. Behaviourally, an *extensionless*
  `#[static_get]` route whose handler returns a bare `String` is served as the
  `text/plain; charset=utf-8` axum declares rather than the `text/html` the old
  heuristic assumed — matching what that same handler already served on the
  dynamic path. Return `Markup` or `Html<String>` for HTML. See
  [the migration guide](docs/migrations/next.md).

- **Ledger: a monotonic head outside the revision rows, and transaction time
  from the database (#2323):** the tamper-evident record ledger allocated each
  revision's sequence number from the rows that survived in
  `_autumn_ledger_revisions`, which left an attacker a window they could close
  themselves. Delete the newest revision, then wait for a *normal* application
  write: the append read `N-1`, re-allocated `N`, chained cleanly onto its
  predecessor and matched the live row, so both the chain walk and the live-row
  cross-check reported intact and the deleted state left no trace.

  A new framework table, `_autumn_ledger_high_water`, keeps a per-record
  high-water mark where deleting a revision cannot reach it. Every append now
  allocates `max(chain head, high-water mark) + 1` and raises the mark in the
  same transaction, so the same attack allocates `N+1` and leaves a **permanent
  gap** that `ledger_verify` reports as `MissingRevision` — whenever it runs,
  not only in the window before the next write. The same mark tells a *wholly
  erased* chain apart from a row that predates ledgering, which the first slice
  had to stay silent about.

  The mark is cross-checked, never believed — on both paths. `ledger_verify`
  compares it with the chain in both directions, so rolling it back
  (`HighWaterBehind`), rewriting it (`HighWaterMismatch`) or deleting its row
  (`HighWaterMissing`) is itself the accusation; and an append that finds the two
  in a state no framework code path can produce **refuses** rather than
  overwriting the evidence. Without that second half, deleting a revision *and*
  the mark and then waiting for ordinary traffic would have the append quietly
  re-create both. A mark merely *behind* the head still writes: that is what a
  pre-#2323 node in a mixed-version fleet leaves, and the next write heals it.
  It raises the bar rather than closing the class — an attacker who can delete
  revisions and rewrite the mark to agree with what survives is still invisible
  from inside the database — so pinning `ledger_head` outside it remains
  required for an audit posture. `ledger_high_water` exports the mark beside it,
  from the same statement. The migration backfills a mark for every chain that
  already exists, so adoption is a plain `autumn migrate` — run it *before*
  rolling out the new binary.

  `recorded_at` no longer comes from the writing host's clock. It is read from
  the database (`clock_timestamp()` on Postgres, `strftime(…, 'now')` on SQLite)
  at the point the append has already read the record's chain head, and clamped
  against the chain's own floor — so transaction time is **non-decreasing along
  a chain by construction**, across node clock skew and host clock steps alike,
  and an as-of query is no longer answered by a revision recorded after the
  instant asked about (up to the commit-visibility lag the guide documents). The
  clamp is bounded: a floor more than an hour ahead of the database's clock
  refuses the write instead of ratcheting the record's transaction time forward
  for good. A chain where transaction time does move backwards is now reported
  as `RecordedAtRegression`, ranked last so a chain written before this change
  cannot mask a truncation. `LedgerBreak` is `#[non_exhaustive]` from here on,
  and `autumn db scrub` refuses to empty one of the two ledger tables without
  the other.

- **SBOMs and signed provenance for framework and app releases (#1615):**
  Autumn could not answer "what exactly is in this artifact, and who built it?"
  at either surface. Its own releases were body-only GitHub Releases with no
  SBOM and no attestation, and the production image from `autumn release init`
  shipped no inventory and `curl`ed the Tailwind binary with no integrity check
  — while `autumn setup` had been SHA-256-verifying the very same download for
  the dev loop all along.

  A new `autumn sbom` generates a deterministic CycloneDX 1.5 document from
  `cargo metadata`: no wall-clock timestamp, a `serialNumber` derived from the
  document's own content rather than randomly generated, components sorted and
  de-duplicated. Same source tree, same bytes. That determinism is what makes
  it a gate rather than a formality — `autumn sbom --verify` regenerates and
  reports a component-level diff (`unexpected component: backdoor@6.6.6`), and
  `--expect-version` ties the document to the version being released.
  `scripts/check-sbom.sh` runs both as the publish gate's new `sbom` job, and
  the same `--verify` runs *again* in `prepare-release` against the artifact
  after it has travelled through the artifact store — the point where a
  substitution or truncation could actually happen. The file that run checks is
  the one attached to the release as `autumn-<tag>.cdx.json`.

  Every release asset — the SBOM, each CLI archive, each `.sha256` — now
  carries a keyless SLSA build-provenance attestation, published before the
  asset is uploaded so an attestation failure stops the release rather than
  leaving unattested assets on a live one. One documented command verifies
  them: `gh attestation verify <asset> --repo autumn-foundation/autumn`.

  Scaffolded apps get the same posture by default. The release Dockerfile
  compiles through `cargo auditable`, so the shipped binary reports the exact
  crate versions inside it with no source tree and no lockfile (`autumn sbom
  --binary /usr/local/bin/my-app`, reading ELF, Mach-O and PE); it obtains
  Tailwind through the checksum-verifying `autumn setup`; and it bakes a
  CycloneDX SBOM into the image at `/usr/share/autumn/sbom.cdx.json` behind an
  OCI label. `autumn build` grows `--auditable` so the embedded single-binary
  path is instrumented too. The `autumn new` Dockerfile's own unverified
  download is verified as well, and now detects its architecture instead of
  hardcoding `linux-x64` — which had been silently installing an unrunnable
  binary on arm64. The generated AWS/GCP/Azure deploy workflows attest each
  pushed image and its SBOM against the image digest, as the last steps in the
  job so a Sigstore hiccup can never block a deploy whose image is already
  pushed.

  The in-image SBOM step is emitted only once the `autumn-cli` version the
  Dockerfile pins can actually run `autumn sbom`. The pin is the scaffolding
  CLI's own version, which between a merge and the next release is an
  already-published version predating the subcommand — emitting it anyway would
  make every `docker build` fail. Until then the image is merely SBOM-less, not
  broken; the auditable binary, the verified Tailwind download and the image
  provenance attestation are all active immediately.

  `docs/guide/supply-chain.md` walks both surfaces end to end, including the
  negative case — tamper with one byte and watch verification fail — because a
  check nobody has seen fail is not a check.
- **One-command plugin install — `autumn plugin add` / `autumn plugin list`
  (#1606):** Autumn had a real plugin seam (the `Plugin` trait, first-party
  plugin crates, a crates.io naming convention, an author-facing
  `autumn plugin-check`) and no consumer-facing tooling at all: using a shipped
  capability meant finding the crate, hand-editing `Cargo.toml`, hand-writing
  the `.plugin(...)` mount, and reading config docs — four places to pick an
  incompatible version or misconfigure the mount. `autumn plugin list` now shows
  every installable plugin with a one-line description and the version
  compatible with the app's `autumn-web` — the five first-party crates plus
  community crates discovered on crates.io through the documented
  `autumn-plugin-<name>` convention (`--json` for machine-readable output,
  `--offline` to skip the lookup). `autumn plugin add <name>` performs the whole
  install: the dependency at a compatible version, the mount spliced into the
  `autumn_web::app()` builder chain, and the post-install steps (config keys,
  follow-up generators like `autumn generate admin`) printed.

  Every refusal is total rather than partial. The version gate runs before a
  single filesystem action exists, so installing a plugin whose supported
  `autumn-web` range excludes the app fails naming both versions with the app
  byte-identical. A second `add` of the same plugin reports it as already
  installed and changes nothing — no duplicate dependency, no duplicate mount.
  And when the builder chain cannot be edited confidently (a heavily customized
  `main.rs`, or a one-line chain with nowhere to splice a call) the command
  writes **nothing** and prints the exact dependency line and mount snippet to
  apply by hand, so it can never leave an app in a non-compiling state. Community
  crates get their dependency written but never an automatic mount: the
  `<Name>Plugin` is derived from the naming convention and printed, because
  nothing outside that crate can verify it. A CI gate installs every first-party
  plugin into a fresh `autumn new` scaffold and requires a green `cargo check`.

- **ACME provisioning against a private CA, and an end-to-end proof of the whole
  flow (#1608):** `[server.tls.acme] directory = { custom = { url = "..." } }`
  was documented for a private CA or a [Pebble](https://github.com/letsencrypt/pebble)
  test server, but the ACME client verified the directory against the platform
  trust store only, so unless that root was installed host-wide the TLS
  handshake failed and every order died before an authorization was created. A
  new `ca_root_path` names the PEM root that signs the ACME directory's *own*
  HTTPS certificate; it replaces the client's trust anchors for the ACME control
  plane only (never for what browsers accept from your site) and is unnecessary
  for Let's Encrypt, whose staging and production API endpoints are both
  publicly trusted. `autumn doctor` grades it as `acme_ca_root`, failing on a
  path that is blank, unreadable, or yields no usable anchor — validated through
  the very `CertificateDer::from_pem_file` + `RootCertStore::add` pair the
  runtime uses — and warning when the file is a bundle (only its first
  certificate is ever installed) or is pinned against a public Let's Encrypt
  directory.

  This closes the gap that kept the order flow itself untested: with a reachable
  private directory, `autumn/tests/integration/acme_end_to_end.rs` now drives a
  real `instant-acme` client over real TLS against an in-process fake CA
  (`acme_fake_ca.rs` — no Docker, no network), which validates HTTP-01 against
  the app's own challenge listener, checks the finalize CSR's SANs against the
  order's identifiers, and issues from its own root with a caller-chosen
  validity window. The suite covers first-boot issuance and the HTTP→HTTPS
  redirect, a **forced near-expiry certificate rotating with no restart while a
  connection opened before the swap keeps serving**, a restart reusing the
  stored account and certificate instead of re-registering or re-ordering, and a
  failed order landing in both `/actuator/health` and the error-reporting seam.
  A new merge-blocking CI lane runs the suite on every push, alongside the
  direct-TLS lane #1603 added, and `acme` joins the gated clippy feature set —
  which is what puts `autumn/src/acme/**` under `-D warnings` for the first
  time, since `--all-targets` lints only what the enabled features compile.

  Also fixes a latent panic on this path: `ca_root_path` reaches
  `rustls::ClientConfig::builder()`, which panics rather than erroring when it
  cannot resolve a process-level `CryptoProvider` — the state any app reaches as
  soon as a dependency enables `aws-lc-rs` alongside autumn's `ring`
  (`telemetry-otlp` alone is enough). Building the ACME client now installs
  `ring` as the default when nothing has set one, keeping any provider the
  application chose deliberately.

- **Personal data that cannot reach a JSON response by accident (#1654):**
  Autumn's protections for sensitive data were all *name*-based and ran at
  runtime — `log/filter.rs` scrubbed a key denylist, `http_client.rs` redacted
  three header names, `gdpr.rs` keyed erasure off table-name strings — so
  renaming a column, adding an endpoint, or routing personal data through a
  differently-named field silently reopened the hole. A `#[model]` column can
  now be annotated `#[classified]`, and the classification is carried by the
  *type*: the field is generated as `Classified<String, CustomerEmailClassified>`,
  a wrapper with no `Serialize`, no `Display`, no `Deref` and no `into_inner`,
  and the model itself loses its `Serialize` derive. There is no expression that
  puts the value where a serializer can reach it, so `Json(customer)` and
  `Json(View { email: customer.email })` are both build failures — with a
  diagnostic that names the offending field and the `Json` sink and says what to
  do about it.

  Releasing the value is declared, not incidental. `autumn_web::declassify!`
  names the column, the sink, a purpose and a non-blank reason, and yields a
  boundary typed to exactly that column — so one field's approved purpose cannot
  release another's. `value.declassify(&BOUNDARY)` takes the value by move (a
  release is a single event, not a permanent widening) and emits an auditable
  record on the `autumn::declassification` tracing target carrying the model,
  field, tier, purpose, sink and reason — never the released value itself.

  `autumn data-flow` emits the diffable manifest: one row per classified column
  listing every sink it is proven reachable to, where an empty reachable set
  means the column cannot leave the process through a gated sink at all.
  `--check` fails the build when it drifts from the committed copy, so a new
  release edge has to be reviewed rather than merged silently. Pass `--release`
  (and the `--features` you ship) to audit the binary you actually deploy: a
  boundary behind `#[cfg(not(debug_assertions))]` exists only in the release
  build.

  The manifest keys each row on the model's module-qualified path, so two crates
  that each define a `Customer` with a classified `email` cannot merge into one
  row, and the Diesel column wrapper carries the column's field marker, so a
  value cannot be converted in as one classified column and back out as another.

  The first slice deliberately stops at one tier and one sink. Name-based log
  and header redaction are untouched and still run; the write structs and the
  generated factory still accept the value (taking personal data *in* is not a
  release) but carry the wrapper too, so the plaintext cannot be moved out of a
  `pub` field into a response view; `Debug` renders `<classified>` everywhere.
  See
  `docs/guide/data-classification.md`.
- **`autumn db scrub` turns a production copy into an anonymized one, and
  refuses to guess (#1602):** the moment `autumn db backup` shipped, a
  production database was one command away from a laptop or a shared staging
  box — PII and all — and the only remedy was hand-rolled `UPDATE` scripts that
  rot the first time someone adds an `email` column. `autumn db scrub` takes
  either a backup artifact (`--artifact`) or the resolved database URL and
  rewrites every PII-classified column with deterministic, constraint-valid
  fake values. Classification comes from the schema, not from a config file
  alone: `#[encrypted]` model columns and tables registered with the GDPR
  anonymize strategy are classified automatically, everything else is declared
  in a checked-in `scrub.toml` — and because the column universe is read by
  introspecting the live database, a column that is neither PII-classified nor
  explicitly marked safe **aborts the scrub** and is listed by name, with a
  paste-ready declaration stanza. That makes "we think staging is clean" a
  CI-verified invariant: `autumn db scrub --check` writes nothing and exits
  non-zero on any unclassified column. Replacements are derived from an `md5`
  over the row's primary key salted with the column name, so a `UNIQUE` column
  stays unique, `NULL`s stay `NULL`, and two columns of one row never collide;
  PII on a primary- or foreign-key column is refused outright, so referential
  integrity survives. Writing refuses outside `dev`/`test` without `--force`,
  the same guard as `autumn db drop`, and every statement for one database runs
  in a single transaction. `--output` re-dumps the scrubbed database as a fresh
  artifact, closing the backup → scrub → restore loop.

  The safety work is where most of the substance is. `#[encrypted]` columns are
  **re-encrypted** under the target's own key rather than overwritten with a
  plain string (which would make every later read of that row fail as malformed
  ciphertext). PII is refused on either side of any foreign key — composite keys
  included — so a natural key another table references is protected, not just
  the referencing column; on `CHECK`-constrained columns, where no fabricated
  value can satisfy the predicate; and on generated columns, which Postgres
  refuses to update at all. Uniqueness is read from every unique index, partial
  and composite included, and a unique column gets a wider `sha256` token so a
  narrow `varchar(n)` cannot truncate into collisions. Statements are
  `public`-qualified with a pinned `search_path` (a tenant `search_path` cannot
  redirect a write), the planned tables are locked for the transaction, row-level
  security and non-`public` schemas are refused rather than silently
  under-scrubbed, materialized views are refreshed in dependency order, and the
  framework-owned tables that keep verbatim copies of app rows — the ledger, the
  version history, the search index, `api_tokens` — are reported and can be
  emptied with `[framework] purge`. See the new
  [Data Scrubbing guide](docs/guide/data-scrubbing.md).

- **`autumn upgrade` now reconciles framework-owned scaffold files, not just
  app code (#1593):** `autumn new` writes about a dozen framework-owned files
  into every project — `Dockerfile`, `.dockerignore`, `build.rs`, `autumn.toml`,
  `tailwind.config.js`, `rust-toolchain.toml`, `clippy.toml`, `rustfmt.toml`,
  the CI workflow, `static/css/input.css` — and those templates keep evolving,
  but bumping `autumn-web` in `Cargo.toml` updates the library, not the project
  skeleton. An app scaffolded on 0.5 therefore kept 0.5-vintage project files
  forever, and the only remedy was diffing a freshly generated throwaway project
  by hand. `autumn upgrade` now renders the current release's scaffold in memory
  and reports a per-file verdict alongside the existing codemod report: `add`
  for a file a later release introduced (a pre-#1492 app is offered
  `rust-toolchain.toml`), `update` for one whose template moved while your copy
  stayed untouched, `conflict` for one you edited, and `removed` for one you
  deleted on purpose. Preview stays the default; `--apply` writes the additions
  and updates and never a conflict.

  Knowing which files you edited needs a baseline, so `autumn new` now records
  one: `.autumn/scaffold.toml` holds the release that scaffolded the project,
  the flags it was created with, and a digest of every framework-owned file as
  Autumn wrote it. Commit it — its value is being the baseline a later checkout
  compares against. Projects created before this feature have no manifest and
  are handled best-effort rather than refused: missing files are still offered,
  and everything that differs is a conflict for review, because "untouched"
  cannot be proven. Digests are taken over LF-normalised text, so a
  `core.autocrlf` checkout on Windows is not mistaken for a rewrite of every
  file.

  Application source is out of bounds throughout: the command never reads,
  writes, or names a path under `src/`, and `Cargo.toml`, `README.md`,
  `tests/`, `migrations/`, `i18n/`, `config/credentials/` and the vendored
  `static/js/` assets are not framework-owned either. `autumn new` and the
  reconciler render from one shared function, so what the scaffold writes and
  what the upgrade considers current cannot drift apart.

  Nothing that cannot be read is ever written: a framework-owned path that is a
  symlink, a directory, or not UTF-8 is reported as a conflict and left exactly
  as it is, rather than being mistaken for a missing file and truncated —
  which matters most for the projects with no manifest, where nothing else
  vouches for the file. Every write is staged in the same directory and
  renamed into place, so an interrupted apply cannot leave a half-written
  `Dockerfile` or a truncated new file — which would be permanent, since a file
  with no recorded baseline is a conflict the command then refuses to repair.
  An updated file keeps its permissions; an added one lands with the mode
  `autumn new` would have given it. The project
  name is read from `[package] name` and validated, never guessed from the
  directory: it is interpolated into `autumn.toml`, the CI workflow and the
  `Dockerfile`'s `CMD`, so a guessed name would render a different scaffold and
  rewrite files that had not actually drifted.

  New: `autumn upgrade --check` reconciles the scaffold files, writes nothing,
  and exits `3` when anything has drifted, so CI can gate on scaffold freshness
  (`1` still means the apply step died partway, and a deliberately deleted file
  does not hold the gate red forever). It prints verdicts without the per-file
  diffs, so a CI log does not accumulate the working contents of `autumn.toml`.
  `autumn upgrade --accept <path>` records a file as yours for good, so a team
  whose `Dockerfile` is deliberately theirs can still hold a green gate rather
  than deleting it. A crate inside a Cargo workspace is not offered the files
  that workspace owns at its root — a crate-local `clippy.toml` shadows the
  workspace's rather than adding to it. Every report ends with a link to that
  release's migration guide, so file reconciliation and API migration are one
  workflow. `--json` carries the whole thing under a `scaffold` key.
  [`docs/guide/upgrading.md`](docs/guide/upgrading.md) covers the workflow end
  to end, including the `git diff` / `git checkout --` revert path.

- **Zero-downtime in-place upgrades with a compile-checked state migration
  (`SIGUSR2`):** a running app can now swap itself to a newly-built binary
  without dropping a connection *or* its in-memory state. On `SIGUSR2` the
  process snapshots and freezes the block of state it designated with
  `AppBuilder::with_live_state(...)`, execs the new binary handing over the
  already-bound listening socket, waits for that build to signal it is serving,
  and only then drains itself — so connections queued on the shared socket are
  picked up by the successor rather than refused, and the drain skips the
  `/ready`→503 flip and prestop grace a real shutdown needs (the address never
  goes away). The new build adopts the snapshot through
  `with_live_state_from::<Old, _>(...)`, whose migration is written with the new
  `state_migration!` macro and is **total by construction**: a struct shape
  whose field mapping is missing fails to build (`missing field … in
  initializer`, with no `..Default::default()` escape hatch in the grammar), and
  an enum shape maps every variant *by name*, so a forgotten variant is a
  non-exhaustive `match` and a `_` catch-all cannot be written at all — four
  `tests/compile-fail/state_migration_*.rs` fixtures pin each refusal. Writes
  attempted after the snapshot are refused with `Err(LiveStateFrozen)` rather
  than silently lost, and every failure path (missing or broken binary, a
  successor that crashes or hangs, a state this build cannot account for, a
  listener that cannot be handed over) abandons the upgrade with the old build
  still serving and its state writable again. Linux/Unix and plain TCP
  listeners; `[server.upgrade] enabled/ready_timeout_secs` configure it. Worked
  example in `examples/hot-upgrade` (whose `live_upgrade` test proves zero
  refused connections and 100% carry-over under sustained load across the
  cutover) and the guide in `docs/guide/hot-upgrades.md`. Issue #1674.

- **Direct HTTPS is now proven end-to-end, not just at the listener:** serving
  TLS in-process (`[server.tls]`) had coverage for the listener itself, but
  nothing exercised the rest of the app surface through it — so "everything
  behaves the same under TLS" was a claim rather than a test.
  `tls_app_surface.rs` now serves the **same** router twice, once over the real
  `TlsListener` and once over plain TCP, and requires the framework probes
  (`/health`, `/live`, `/ready`, `/startup`) and `/actuator/health` to match on
  status, body, and content type; it drives the inbound
  request timeout (a slow handler still 503s, a fast one still doesn't), an SSE
  stream (events arrive incrementally and outlive the request deadline), a
  `wss://` WebSocket echo, and a graceful shutdown that drains an in-flight
  HTTPS request. A new blocking CI lane runs the whole `tls` suite, which the
  workspace `cargo test` — where `tls` is off by default — never compiled.
  Renewal is covered the same way: the mtime-polling reloader moved out of
  `app.rs` into `autumn_web::tls::CertReloader`, so a test can rewrite the cert
  and key on disk and watch the served certificate change with the site up and
  no restart. `autumn/src/tls.rs` also joins the determinism seam gate, so its
  one deliberate wall-clock read (certificate validity) stays the only one in
  that module.
  Issue #1603.

- **The release-image boot gate now covers an HTTPS boot** (a new
  `https-target` job): it builds the generated image with the `tls` feature on,
  boots it with a self-signed test certificate supplied through
  `AUTUMN_SERVER__TLS__*`, and requires an HTTPS `/health` + `/actuator/health`
  200 validated with `--cacert` (not `-k`), that plain HTTP on the same port
  does *not* answer, and that the container's own HEALTHCHECK reaches
  `healthy`. `docs/guide/tls.md` gains the matching "Serving HTTPS from the
  release image" walkthrough — including that the `tls` feature must be a
  default feature for the image's `cargo build --release` to link it — and a
  "What behaves the same under TLS" section naming the two things that
  deliberately differ (no Unix socket, no in-place upgrade handoff). Issue
  #1603.

- **ci:** `autumn-cli`'s consolidated `cli_tests` binary now gets the same
  discovery sweep as the `autumn` crate's `integration_tests` binary
  (#1945): a bare `--ignored` run over `cli_tests` in ci.yml's
  Docker-dependent-tests step, so a new `#[ignore = "requires Docker
  (testcontainers)"]` test in any `autumn-cli/tests/integration/*.rs` module
  runs in CI automatically. Before this, only two hand-picked filters
  (`offsite`, `db_scrub`) ran anything from that binary, leaving 46 Docker
  tests across 8 modules (`db.rs`, `db_pull.rs`,
  `generate_lock_version_postgres.rs`, `generate_references_postgres.rs`,
  `migrate_down.rs`, `schema_migrate.rs`, `schema_pull.rs`, `test_command.rs`)
  dark since they were written — PR #1985's noted follow-up. The sweep's
  `--skip` list routes the binary's other `#[ignore]`d tests — the ones that
  scaffold and cargo-check/build/run a fresh generated project instead of
  touching Docker — to `generator-conformance.yml`, where 15 of them (across
  `api_scaffold`, `cloud_native_scaffold`, `generate_position_scaffold`,
  `scaffold_belongs_to`, `scaffold_bulk_delete`, `scaffold_rich_text`,
  `scaffold_search`, `scaffold_trash`, `seed_model_linking`, `serve`, and two
  in `scaffold_form_for`) are now named there for the first time, closing the
  same gap for that half of the binary. The sweep also `--skip`s the
  pre-existing `generate_json_postgres.rs` Docker test, which already ran in
  `generator-conformance.yml` before this change, so it isn't doubled up.
  Two new `autumn-cli/tests/integration/repo_hygiene.rs` tests guard both
  halves of the wiring going forward. [no-plugin]

### Changed

- **ci: the Docker/testcontainer sweep runs as its own `Test (Docker)` job
  instead of the last step of `Test (ubuntu-latest)`:** as step 16 of that job
  it inherited a disk already filled by the whole workspace build plus eight
  feature-flipped rebuilds, and died compiling its own dependencies
  (`bollard`, `bollard-stubs`, a re-linked `autumn-web`) with `No space left on
  device` and 23–41 MB free — before a single test body ran, with steps 1–15
  and both the macOS and Windows legs green. The two steps move verbatim (the
  diff is purely additive: a job boundary inserted in front of them), so the
  same commands run the same sweep, on a runner whose disk they are the only
  claimant of. Branch protection still names only the per-OS `Test (…)` checks,
  so `Test (Docker)` needs adding there to block merge again. [no-plugin] —
  CI-only; no API, behaviour or feature change. (#1747)
- **acme:** [no-plugin] Internal cleanup, no behavior change (#1864): the owner-only
  temp-write-then-rename idiom, previously duplicated between
  `acme::store::FsAcmeStore` and the failure-capture capsule writer, is now
  one shared helper; `FsAcmeStore` gained a `find_cert_for_domains`/
  `list_certs` API that `autumn doctor`'s ACME preflight reuses instead of
  re-deriving the on-disk cert-path layout by hand; and the ACME renewal
  spawn site queries fleet-distribution through a named
  `SchedulerCoordinator`/`SchedulerBackend` predicate instead of matching the
  scheduler config enum inline.
- **ci:** the test suite is now **sharded across runners** instead of running as
  one job per OS [no-plugin] — CI scheduling only; it adds no framework surface
  an agent could reach for, and the notes for humans editing tests live in
  CLAUDE.md rather than the plugin. On the 2026-08-26 trunk run
  `Test (windows-latest)` alone was 128 minutes and *was* the critical path of a
  2h27m run. Two things dominated, both measured from that run's logs. First,
  `compile_fail::` (trybuild) was 37.1 of the 46.8 minutes the consolidated
  `integration_tests` binary spent running on Windows, and it was the *tail* —
  the binary finished 0.2s after trybuild did, on every OS. Each case shells out
  a nested `cargo` build and trybuild serialises them behind a project-dir lock,
  so only more runners make it faster. Second, the eight non-default feature
  lanes ran in sequence in that same job, ~44 minutes of pure recompilation on
  Windows despite being independent builds.

  `test` is now four job families running side by side — `test` (the workspace
  suite), `trybuild` (four shards), `test-features` (one job per feature set)
  and `test-docker` — behind one aggregate `Test suite` gate. First fully-green
  sharded run: **2h27m → 1h57m**. `trybuild`, `test-features` and `coverage`
  were then narrowed further: the first two to Linux only (a trybuild golden is
  pinned to the rustc version, not the OS; a feature lane asks about a feature,
  not a platform), and `coverage` — by then a co-equal 55-minute tail — split
  into four lanes by feature set, each uploading under its own Codecov flag.
  That took the workflow from 47 expanded jobs to 32. Those three changes land
  after the 1h57m measurement, so the new total is not yet measured.

  Test-layout consequences, all of which keep every test running and
  merge-blocking: `compile_pass_tests` split into `_a`/`_b` (a disjoint split of
  the same fixture list, no fixture added or removed); the `sim_*` determinism
  modules got a single-threaded second step, because removing trybuild freed the
  libtest thread pool and the remaining ~1880 tests went from trickling through
  trybuild's gaps (863s) to full parallelism (61s), enough to flip them on a
  4-vCPU runner; and `capsule_cache_effect` moved to its own binary, because it
  installs a process-global cache that any concurrent `TestApp::build` clears.
  Every shard runs `cargo test --workspace` deliberately — the trybuild fixture
  list is `#[cfg(feature = ...)]`-gated, so narrowing a shard would silently
  compile fewer fixtures and still report green — and each asserts a non-zero
  pass count, because `cargo test` exits 0 when a filter matches nothing.
  **Branch protection must be repointed** from the per-OS `Test (…)` checks to
  `Test suite` (`test-gate`), the one name that stays stable as shards come and
  go.
- **plugin-conformance:** **Breaking:** `plugin_conformance::ConformanceConfig`
  gains a `contract` field and is now `#[non_exhaustive]`, so it can no longer
  be built with a struct literal — use `ConformanceConfig::new(name)` and the
  fluent setters, which are unchanged. `#[non_exhaustive]` lands with the field
  deliberately: it is what lets a later release add a check's configuration
  without breaking every plugin's test suite again. `autumn plugin-check` also
  now **fails** a plugin that declares no `Plugin::contract`; the plugin itself
  compiles and runs unchanged. Part of #1601; see the
  [migration guide](docs/migrations/next.md).
- **`#[repository]`'s write paths are no longer re-emitted at every call site,
  cutting a ledgered repository's expansion by 65%:** the `db` gate (#2309)
  removed `autumn-macros`'s own compile time for a no-database app, but a
  DB-backed app re-enables `db` and pays none of it back — its cost is in
  compiling what the macros *emit*, once per `#[repository]`, in every crate
  that declares one. Measured per invocation, a plain repository expanded to
  ~72 KB of Rust and a `ledgered` one to ~508 KB; autumn's own consolidated
  `integration_tests` binary compiles 60 of them. In a release build a
  `#[repository]` costs roughly three times what a `#[model]` does.

  Three write paths that never depended on the model have moved into the runtime,
  where they compile once for the whole program instead of once per repository:

  * `autumn_web::version_history::append_version_history` — the version-history
    INSERT and its pg/`SQLite` fork (#1996), previously inlined at each of the
    macro's ~30 mutation sites;
  * `autumn_web::ledger::append_revision` — the whole ledger append: the
    chain-state read, the #2323 high-water cross-checks, the sequence
    allocation, the hash and the two writes. Each call site had been expanding a
    chain-state struct and two `QueryableByName` derives of its own;
  * `autumn_web::repository::{dependent_restrict, dependent_nullify,
    dependent_delete_all}` — three of the four `dependent(...)` cascade arms
    (#1369), which were already pure dynamic SQL over a table name and a foreign
    key held in runtime `&str`s.

  Alongside them, `autumn_web::__private::maybe_immediate_transaction` replaces
  the `const HAS_COUNTER_CACHES` guard pattern that emitted the **whole mutation
  body twice** — once transactional, once bare — at seven sites per repository
  (#1325). It takes the const as a runtime flag and opens a transaction only when
  it is set, so the body is emitted once. Nothing changes at runtime: a model with
  no counter cache still issues its single statement with no `BEGIN`/`COMMIT`.

  Resulting expansion per `#[repository]`:

  | declaration | before | after |
  | --- | --- | --- |
  | plain | 72.7 KB | 68.9 KB (-5%) |
  | `soft_delete` | 82.0 KB | 77.2 KB (-6%) |
  | `tenant_scoped` | 94.0 KB | 89.3 KB (-5%) |
  | `versioned` | 144.3 KB | 110.7 KB (-23%) |
  | `ledgered` | 508.5 KB | 174.6 KB (-66%) |

  What deliberately stays generated is the typed CRUD — `find_by_id`, `save`,
  `update`, `page`, `list`, and the `dependent = destroy` cascade arm. Diesel's
  typed DSL is what checks those queries against the app's `schema.rs` at compile
  time; rewriting them as dynamic SQL would trade the framework's main
  correctness guarantee for build speed. Only code that was *already* dynamic
  SQL, or already erased its per-column typing behind `&'static` specs and `fn`
  pointers the way `counter_cache_after_insert` does, was moved.

  No API changes and no behaviour changes — same statements, same order, same
  transactions, same errors. A new `repository_expansion_stays_within_budget`
  test asserts a per-declaration ceiling on the expansion so a statement inlined
  back into a per-call-site fragment fails the build rather than quietly
  regressing; the SQL-shape assertions that used to live in `autumn-macros` moved
  to `version_history` and `ledger`, next to the statements they describe.

  [no-plugin] — nothing here is agent-facing. Every function this adds is
  `#[doc(hidden)]` and documented as semver-exempt ("a runtime support function
  for code generated by Autumn proc macros; do not call it directly"), and the
  `ledgered` / `dependent(...)` surface the plugin does document — the attribute
  spelling, the methods it generates, the tables it writes — is unchanged. What
  moved is where the framework emits the code from, not what an app writes or
  what it does.

- **The `Listening` log line now reports the address actually bound** rather
  than the one configured, so `server.port = 0` (and a socket inherited from an
  in-place upgrade) shows the real port instead of `0`. Issue #1674.

- **Replay capsules now record every framework effect a failing request
  touched, and one command turns a capsule into a committed regression test:**
  [#1634](https://github.com/autumn-foundation/autumn/issues/1634) extends the
  deterministic replay capsules from
  [#1598](https://github.com/autumn-foundation/autumn/issues/1598) past the
  inbound request, the database and the clock. A capsule now also carries the
  **outbound HTTP** exchanges the run made (so outbound webhook deliveries come
  along, they send through the same client), the **jobs** it enqueued, its
  **cache** reads and writes, the **mail** it sent, the **tenant** it resolved,
  and every **random draw** it took — each captured at the one choke point every
  code path funnels through, so a capsule cannot miss an effect because a
  handler reached it a different way. Replay serves all of them from the
  capsule with the same no-live-effects posture the database tape has: an
  outbound call is answered from the recording rather than dialled, an enqueue
  is asserted and never written to a queue, mail is asserted and never
  delivered (a recorded *delivery failure* is reproduced as one, so a handler
  whose bug is that it mishandles a suppressed recipient meets it again), and a
  framework-minted identifier — a session id, a CSRF token, a
  request id, a job id — reappears byte-for-byte, because Autumn records the
  drawn bytes rather than a seed (production runs the OS CSPRNG, which has none,
  and a re-seeded stream would mint different UUIDs than the ones the capsule's
  own SQL binds were bound with). An effect the replayed code performs that the
  recording never did — and a recorded effect it never performs — are both
  divergences, exactly as they already were for SQL. A failure *inside* a job
  execution produces a job-scoped capsule replayable the same way.
  `autumn capsule test <capsule>` converts a triaged capsule into a
  `#[tokio::test]` in the app's consolidated integration suite: the capsule's
  bytes are copied **verbatim** into `tests/capsules/` — nothing re-derived, so
  whatever redaction removed stays removed — a test is generated beside it, both
  are registered in `tests/integration/mod.rs`, and a router hook is scaffolded
  once and then left alone. The generated test drives the same
  `capsule::execute` engine `autumn replay` does (so the two can never disagree
  about what a reproduction is) and runs under plain `cargo test` with **zero
  live dependencies** — no network, database, queue or Docker, including for
  DB-touching capsules, whose pool comes from the in-process stub server
  rebuilt out of the recorded wire frames. `autumn capsule verify` is the
  whole-corpus mode; an empty corpus is a failure, never a vacuous pass. Effects
  are redacted through the same `[log] filter_parameters` list the inbound
  request is — an *outbound* `Authorization` header carries a downstream
  credential exactly the way an inbound one carries the caller's — and the
  `redacted_keys` manifest names every masked effect location. See
  [Failure Capsules](docs/guide/failure-capsules.md).
  **Breaking:** the capsule `format_version` bumps `2 → 3`, so a capsule
  recorded by an older Autumn is **refused** rather than replayed with every new
  seam empty, and `capsule::execute` takes a `ReplayFixtures` (the clock, the
  entropy source and the effect tape from one capsule) in place of its
  `Option<&ReplayClock>`. Capsules on disk are not migrated: replay them with
  the version that wrote them, or re-record. See
  [the migration guide](docs/migrations/next.md).
  A capsule commits to a verdict only where it can be honest about one, so
  four cases refuse or declare themselves incomplete rather than grade a run:
  an outbound call or a mail send is served from the tape only when its
  *contents* match too — same endpoint but a different amount, or the same
  recipients but a different letter, is a divergence rather than a clean
  reproduction (a mail *sender* counts only when the replayed run chose one,
  since a message that names no `from` inherits `[mail] from` at send time and
  a replay boots without mail configuration); a job capsule whose payload `[log] filter_parameters` masked is
  refused, because a handler is handed its payload verbatim and would parse the
  `[FILTERED]` placeholder; an effect whose future was cancelled before it
  finished (a losing `tokio::select!` branch) marks the capsule incomplete
  instead of persisting a backend failure the run never had; and a run that
  enqueued inside its own transaction (`enqueue_on_conn`) does the same, since
  that enqueue is also a job-row INSERT on the database tape that replay can
  serve but never issue. A **panicking** job now leaves a capsule whatever its
  attempt number: all three backends dead-letter a panic immediately, so its
  first attempt is also its last, and gating capture on the final attempt meant
  the job failure most worth a capsule never produced one.
  Two further rules keep a verdict honest. A recorded **failure** is rebuilt
  with its own error variant and its exact recorded text — no replay marker —
  so a handler that branches on `ClientError::CircuitBreakerOpen` or
  `MailError::AllRecipientsSuppressed` takes the branch it took in production,
  and one that propagates the error produces the same outcome message rather
  than a spurious mismatch. And a **mail send** is compared on everything a
  recipient would notice: both halves of a multipart body, reply-to,
  `List-Unsubscribe`, caller-set headers, and each attachment by name, type,
  size and SHA-256 (never its bytes — an invoice has no business being copied
  into a capsule). `autumn capsule verify` now also checks that every committed
  capsule still has a generated test wired into the consolidated suite: it runs
  the corpus by name filter, so a deleted test would otherwise be skipped in
  silence while the corpus reported that it replays clean.
  An `enqueue_at` deadline is compared as a deadline rather than as a delay
  derived from it, so a job rescheduled to a different instant is noticed; a
  capsule whose outbound *response body* or *cache hit* was masked is refused,
  since that data reaches the handler as input rather than as something
  compared; the mail redaction sweep covers reply-to, `List-Unsubscribe`,
  caller-set header values and attachment filenames, so a value filtered
  elsewhere cannot sit in the clear one field over; and a response's final
  post-redirect URL is recorded and restored, for handlers that inspect
  `Response::url()`.
  Redaction reaches three places it had missed, two of which leaked: the URL a
  redirect landed on (an OAuth callback carries `access_token` in its query as
  a matter of course) and an enqueue rejection's free-form error text, both of
  which could hold a value filtered everywhere else in the capsule. The third
  is the resolved tenant, which — like a response header and the response body
  — is *input* rather than compared data, so a masked one now refuses instead
  of running the request under a tenant production never resolved. A cache hit
  whose value will not serialize marks the capsule incomplete rather than
  recording as a miss the handler never took. And `autumn capsule test` now
  writes the Cargo test target that compiles the generated suite: Cargo does
  not descend into `tests/` subdirectories, so without a top-level `mod
  integration;` the generated tests were never built and `cargo test capsule_`
  matched nothing — which `autumn capsule verify` now reports as unusable
  rather than as a clean corpus.
  Replay also dispatches a recorded job through the application's
  `JobInterceptor` when one is registered, since that is part of how the
  recorded run executed; an `enqueue_after_commit` is recorded where the
  handler registers it, because the deferred callback runs on a task that does
  not inherit the capture scope and the enqueue would otherwise be missing from
  the capsule and diverge on every faithful replay; the generated support
  module names Axum through `autumn_web::reexports::axum`, which is the only
  path that resolves in an application that does not depend on Axum directly;
  and a non-UTF-8 outbound response header is preserved lossily rather than
  recorded as empty.
  A cache write records the expiry it asked for and compares on it, since a
  five-second entry and a permanent one are different mutations; a write whose
  value will not serialize marks the capsule incomplete, as the matching read
  already did; an attachment filename compares through the redaction wildcard
  while its type, length and digest stay exact, so a masked filename does not
  report a divergence against unchanged code; and outbound request and response
  headers are charged against `max_capsule_bytes`, which they had escaped.

- **`autumn_web::redis_tls` (new public module):** `open_client` is the
  Redis client constructor every Autumn subsystem now uses — it installs the
  rustls `CryptoProvider` a `rediss://` URL needs before rustls can be asked
  to resolve one. `ensure_tls_crypto_provider` exposes just that step for code
  that builds a client another way, and `redact_url` masks the password in a
  Redis URL before it is logged. `redis` is re-exported as
  `autumn_web::reexports::redis` so callers can name the returned
  `redis::Client` without adding their own dependency. Issue #2172.

### Security

- **`#[secured]`, `#[step_up]`, and `#[throttle]` now reject a request before
  its body is ever parsed (#1668):** all three guard checks used to run as
  statements inside the generated handler body, which Axum only invokes after
  every extractor — including the body extractor (`Json`/`Form`/`Multipart`)
  — has already succeeded. An over-limit or unauthenticated request with a
  malformed body got the extractor's `400`/`422` instead of the guard's
  intended `429`/`401`/`403`, masking the guard's outcome, and the server paid
  the cost of parsing and buffering the body (worst for uploads) before a
  `#[throttle]`-gated route ever got to shed the load. Each macro now emits a
  `FromRequestParts` gate — a small generated type inserted as the handler's
  first parameter — so the check runs and can reject the request before
  Axum's extractor pipeline ever reaches the body extractor, relying on
  Axum's guarantee that every `FromRequestParts` extractor resolves,
  left-to-right, strictly before the trailing `FromRequest` one. The macro
  invocation syntax and handler signatures at call sites are unchanged; a
  route's role/scope markers still surface in the generated OpenAPI document
  exactly as before. Fixing this also surfaced a related idempotency-replay
  gap: when `#[authorize]` is written above one of these guards, a stale scan
  could let the guard's new pre-body gate wrongly claim replay-serving for
  itself, which — now that the gate runs before the body — would have let a
  retried mutation replay its cached response without `#[authorize]`'s policy
  check ever re-running. That gap is closed in the same change.

- **The idempotency cache is now partitioned by the resolved tenant:** an app
  that turned on Autumn's multi-tenancy (`[tenancy] enabled = true`) *and*
  `AppBuilder::idempotent()` shared one cache slot between tenants. The storage
  key namespaced by method, request target and a cookie-session principal
  digest — and for `header`, `subdomain` or `jwt` tenancy two requests from
  different tenants differ in none of those (the tenant header and `Host` are
  deliberately excluded from the key, and a token-authenticated API has no
  cookie session). A request that resolved to tenant B, carrying the same
  `Idempotency-Key` and body as an earlier tenant-A mutation, was answered with
  **tenant A's stored response**: macro-generated routes replay *through* their
  own guards, which check roles and scopes but never tenant identity, so the
  handler — and every `tenant_scoped` repository predicate inside it — never
  ran, and tenant B's own write was silently suppressed. The router already
  forced the fail-closed replay path whenever an app resolved tenants in its own
  `AppBuilder::layer`; the framework's own tenancy middleware was not covered by
  that check. The key now carries the tenant as the tenancy middleware resolved
  it (from the `CURRENT_TENANT` task-local, not from the wire, so a legitimate
  same-tenant retry still replays). Apps that do not use tenancy compute
  byte-identical keys to before, so no cached entry is invalidated on upgrade.
  For `[tenancy] source = "session"`, a handler that itself changes the
  session's tenancy key (an organization switch) now has its deferred replay
  alias keyed by the *finalized* tenant rather than the one resolved before the
  handler ran, so a retry after such a switch still replays instead of
  re-running the mutation. See
  `docs/security/2026-09-02-idempotency-tenant-scope/`.
- **Outbound webhook delivery now dials `target_url` through the SSRF-safe
  path:** `WebhookSubscription::target_url` is a subscriber-chosen
  destination — the outbound-webhooks guide describes it as "a consumer's
  registered endpoint" — and the `autumn_webhook_delivery` background job
  posted to it with a plain `Client::post()`, which carries none of the
  private/loopback/link-local/CGNAT/cloud-metadata deny-list
  `Client::get_ssrf_safe()` already enforces elsewhere in the framework. An
  app following the documented pattern (letting its own users register a
  webhook receiver URL) let any such user point delivery at an internal
  service, the app's own database host, or a cloud metadata endpoint, with
  Autumn's own backend making the request on their behalf. `get_ssrf_safe`
  itself only ever built a `GET`; the fix adds a general
  `RequestBuilder::ssrf_safe()` chainable on any verb (`get_ssrf_safe` is now
  defined in terms of it, unchanged in behavior) and applies it to the
  webhook delivery request. Delivery to a blocked destination now fails
  closed with `SsrfBlocked` before any socket opens and is recorded/retried/
  DLQ'd exactly like any other transport failure. **Compatibility note:** an
  app relying on `target_url` reaching a private address (e.g. an
  internal-only receiver during development) will see those deliveries start
  failing after upgrade — this is the intended effect of closing the gap. See
  `docs/security/2026-09-03-webhook-ssrf/`.

### Performance

- **`MemorySearchBackend::keyword_search` no longer re-tokenizes every
  document's fields on every query:** `score` (in `autumn-search/src/memory.rs`)
  called `tokenize` — which allocates a `String` per token via
  `str::to_lowercase` — on every indexed field of every document, on every
  single `keyword_search` call. Profiling a realistic 5,000-document,
  two-field (~206 words/document) corpus with `valgrind --tool=callgrind`
  found this re-tokenization (the scan loop itself plus `str::to_lowercase`)
  accounted for ~66% of the call's instructions. A document's tokens don't
  change between searches, only between writes, so `MemorySearchBackend` now
  tokenizes each document's fields once, when it is written (`StoredDocument`
  in `memory.rs`), and every later `keyword_search`/`score` call reuses the
  cached tokens instead of recomputing them. Purely an internal
  representation change to the in-memory reference/dev backend — no public
  API moved and ranking behavior is unchanged (same 128 `autumn-search` tests
  pass unmodified in assertions). New harness:
  `autumn-search/benches/keyword_search.rs`. Measured on the same machine, one
  session: instructions (callgrind, 5 queries over the corpus) 12,029,984,210
  → 1,820,921,069 (**-84.9%**); marginal allocation blocks/query (dhat)
  1,038,586 → 8,586 (**-99.2%**); marginal allocation bytes/query (dhat)
  6,563,220 → 530,398 (**-91.9%**).

### Fixed

- **examples/reddit-clone: concurrent identical `/submit`s could duplicate a
  post's slug and make its permalink silently serve a different post (issue
  #2544):** `unique_slug()`/`unique_slug_excluding()` proved uniqueness with a
  `SELECT COUNT` before the `INSERT`/`UPDATE` that relied on it — a
  check-then-act race two concurrent submits (a double-click, or a
  flaky-network auto-retry) could both win, landing two posts on the same
  `(subreddit_id, slug)`. Nothing at the database level backed that
  invariant (`posts.slug` had only a plain, non-unique index, unlike
  `subreddits.slug`/`users.username`), so once duplicated, `show()`'s
  unordered `.filter(slug...).filter(subreddit_id...).first()` returned an
  arbitrary one of the two forever — the other post's own permalink now
  silently served someone else's title, body, and comments with a `200` and
  no error. Fixed with a composite `UNIQUE (subreddit_id, slug)` constraint
  (migration `20260906163932_posts_slug_unique_per_subreddit`) plus a retry
  loop in `submit`/`update`: the existing `SELECT`-based guess stays as a
  fast path, but a losing insert/update now comes back as a unique-violation
  on that named constraint, which the loser catches and retries with the
  next candidate slug instead of colliding with the winner. Regression-tested
  by driving the real compiled binary with 10 fully concurrent, identical
  `/submit` requests and asserting no duplicate `(subreddit_id, slug)` pair
  survives (`tests/post_slug_race_e2e.rs`).

- **`#[query_budget]` silently missed queries issued through a handle bound
  via an async/fallible accessor (e.g. `let mut conn = self.conn().await?;`),
  a real shape in `PostgresSearchStore::write_documents`:** neither
  `Analyzer::expr_is_handle` nor `chain_root_is_handle` peeled
  `Expr::Await`/`Expr::Try` before checking whether a call was one of the
  recognized handle accessors (`db`, `repo`, `repository`, `pool`, `conn`,
  `connection`), so `conn` never entered the tracked-handle set and every
  later query issued through it (e.g. a diesel-async
  `query.execute(&mut conn)` inside a loop) went uncounted with **no
  diagnostic at all** — worse than the analysis's own "never assume
  query-free" contract, which is meant to *report* what it cannot prove, not
  silently drop it. `expr_is_handle` now recognizes `self.conn().await?`
  through a new, deliberately narrower `awaited_expr_is_fresh_handle` helper
  that only fires on the `?`-unwrapped shape — a bare `self.conn().await`
  (no `?`) still yields the `Result` itself, not the handle, and is not
  promoted, so a later `result.is_err()`/`.unwrap()` is not miscounted as a
  query. [no-plugin] — analysis-only fix inside `autumn-macros`; no API
  change.
- **macros: stacked `#[secured]`/`#[step_up]`/`#[throttle]` above a route
  attribute broke instead of composing (issue #2516):** #1668 moved each of
  these three body guards' checks out of the handler body and into a
  `FromRequestParts` gate — a hidden struct + trait impl now emitted as
  sibling items ahead of the (rewritten) handler function, rather than
  wrapping the body in place. Every macro downstream of one of these guards
  — the route macro (`#[get]`/`#[post]`/etc.), and the three guards
  themselves when stacked on each other — still assumed its `item` input was
  exactly one function, so a guard written above another guard or the route
  attribute handed the next macro a multi-item stream it rejected outright
  with a confusing "route macros can only be applied to functions" error,
  silently on every PR touching route macros regardless of whether the
  guards were involved (the failure is in the default, always-compiled
  `autumn-macros` unit suite). Fixed by teaching every one of those call
  sites (`parse::split_leading_items_and_fn`, shared by the route/static/ws
  macros and reused directly by the three guards) to recover the trailing
  function from a longer item sequence and re-emit everything before it
  verbatim, so an earlier guard's gate type is never dropped. Separately,
  `#[step_up]`/`#[throttle]`'s move to a gate had also stopped emitting
  their `__AUTUMN_STEP_UP_MAX_AGE`/`__AUTUMN_THROTTLE_ROUTE_ID` marker
  consts into the handler body, which is what lets the route macro tell a
  real guard's `__autumn_inner` return-type wrapper apart from unrelated
  code (#1677) — restored by emitting an inert copy of each into the body
  alongside the real one in the gate, mirroring `#[secured]`'s existing
  `role_scope_consts`/`markers` split.

- **macros: `#[secured]`/`#[step_up]`/`#[throttle]`/`#[authorize]` stacked
  with `#[static_get]` is now a compile error, not a false "protected"
  certification:** the fix above taught `#[static_get]` to accept a guard's
  leading gate items via `parse::parse_async_handler_with_leading_items`,
  and an initial pass had it read the guard's role/scope marker back off the
  handler via `api_doc::extract_secured_info(&input_fn)` — mirroring
  `crate::route` — so a `#[secured("admin")]`-guarded static route would
  report `secured: true` to `routes audit` instead of the previous hardcoded
  `secured: false`. A follow-up Codex pass caught that this was actively
  wrong, not just incomplete: cached SSG/ISR responses are served by the
  static-first middleware *before* the inner router (session, auth) is ever
  reached (`AppBuilder::static_gate`'s doc comment spells this out), so the
  guard only ever runs on the rare synchronous render/revalidate call, never
  on a cache hit — the overwhelming majority of live traffic to a cached
  page. Certifying `secured: true` there made `routes audit` wrongly attest
  the page as protected when an anonymous request against a warm cache entry
  gets the cached HTML unauthenticated either way. `#[static_get]` now
  rejects the combination outright, in both attribute orders, and the error
  points authors at `AppBuilder::static_gate` — the gate actually built to
  protect pre-rendered pages (Codex review on #2513, P1). That rejection's
  first version detected a guard expanded *above* `#[static_get]` by
  checking for a leading sibling gate item — a shape only a test that
  hand-concatenates one macro's raw multi-item output into another's input
  can produce; the real compiler never bundles a sibling item alongside the
  one it hands to the next attribute macro in a stack (confirmed by
  `param_helpers::extract_fn_item`'s own doc comment, added for exactly this
  reason elsewhere in this crate). So in genuine compiled code that leading-
  item check could never fire, and the rejection for this — the more
  natural — attribute order silently never worked, an eighth Codex finding
  on #2513 caught. Fixed by checking what actually does survive onto the
  single function the real compiler hands `static_get_macro`: the guard's
  own pre-body gate parameter for `#[secured]`/`#[step_up]`/`#[throttle]`
  (`param_helpers::has_any_guard_gate_param`), and the role/policy-check
  body marker for `#[authorize]`, which inserts no such parameter
  (`api_doc::extract_secured_info`, the same recovery the `#[get]`/`#[post]`
  route macro already relies on for this scenario). The accompanying unit
  tests were corrected the same way — sliced through
  `param_helpers::extract_fn_item` rather than fed a raw multi-item macro
  output — so they exercise the shape the compiler actually produces instead
  of a synthetic one, and would have caught this gap themselves.

- **macros: `#[secured]`/`#[step_up]`/`#[throttle]` above `#[ws]` never
  actually worked, and generated silently-broken code rather than a clear
  error:** `ws_macro` builds a two-function wrapper that calls the user's
  handler by hand, and for each non-`AppState` parameter it echoed that
  parameter's *pattern* straight back as the call argument. A guard expanded
  above `#[ws]` inserts a leading `_: __AutumnXGate` parameter, and `_` is a
  pattern, not a valid expression — `#fn_name(_)` does not compile (Codex
  review on #2513, P1). Fixing that forwarding surfaced a deeper,
  pre-existing incompatibility Codex caught on the very next review pass:
  every one of those three guards unconditionally rewrites the wrapped
  function's return type to `Response` and threads its original return value
  through `IntoResponse::into_response`, which cannot hold for a `#[ws]`
  handler — it returns `impl WsHandler` (a plain closure), not something
  `IntoResponse`. Properly supporting the combination would mean teaching
  all three guard macros to special-case a WebSocket handler's return type,
  a cross-cutting redesign out of scope here. `#[ws]` now rejects the
  combination outright with a purpose-written compile error (mirroring the
  existing `#[edge]`-on-`#[ws]` rejection) instead of ever emitting code that
  fails to compile deep inside guard-generated internals; the error explains
  the incompatibility and suggests checking authorization via an extractor
  inside the upgrade handler instead. That rejection initially only checked
  for an already-*expanded* guard above `#[ws]` (a non-empty leading-items
  stream); a third Codex pass caught that the *other* attribute order —
  `#[ws]` outermost, the guard still a live, unexpanded attribute below it —
  slips past that check entirely (nothing has expanded yet, so there are no
  leading items) and generates the same silently-broken code, just one macro
  expansion later. The rejection then also scanned the handler's still-live
  attributes for `#[secured]`/`#[step_up]`/`#[throttle]`/`#[authorize]` — but
  a fourth Codex pass caught that `#[authorize]` above `#[ws]` (already
  expanded) slips past *both* checks: unlike the other three guards,
  `authorize_macro` emits no separate `FromRequestParts` gate sibling item
  (so leading items stay empty) and it removes its own attribute once
  consumed (so the live-attribute scan finds nothing either). Rather than
  keep chasing each guard's particular expansion shape, the rejection now
  checks the actual invariant directly: all four guards rewrite the return
  type to the exact same `-> Response` (confirmed identical across all four
  guards' source), which a legitimate `#[ws]` handler — required to return
  `impl WsHandler` — never would. Separately, `#[ws]`'s `ApiDoc` had the
  same hardcoded `secured: false, required_roles: &[]` gap `#[static_get]`
  had for the (still-supported) case of a live `#[authorize]` attribute or
  policy check — fixed the same way, via `api_doc::extract_secured_info`
  (Codex review on #2513, P2). That return-type check itself had a false-
  positive gap a fifth Codex pass caught: it matched a `Response` return
  type by its *last path segment only*, so a `#[ws]` handler legitimately
  returning some unrelated user type merely *named* `Response` (e.g.
  `my_crate::Response`, implementing the public `WsHandler` trait) would be
  misclassified as guard-incompatible and rejected outright. Fixed by
  matching the guard's exact fully qualified return path segment-by-segment
  (`::autumn_web::reexports::axum::response::Response`) instead of just the
  final identifier.

- **macros: `#[static_get]`/`#[ws]` missed a `#[secured]`/`#[step_up]`/
  `#[throttle]`/`#[authorize]` guard hidden behind
  `#[cfg_attr(predicate, ...)]`:** both rejections' "guard still a live,
  unexpanded attribute below the route macro" check compared each
  attribute's own path against the guard names directly, so it correctly
  caught a bare `#[secured("admin")]` written below `#[static_get]`/`#[ws]`
  but missed the identical case written as
  `#[cfg_attr(feature = "auth", secured("admin"))]` — `cfg_attr` is a
  built-in attribute the compiler does not resolve until after every
  attribute macro has finished expanding, so the outer route macro sees a
  live `cfg_attr` attribute, not `secured`, and let the combination through
  uncaught (ninth Codex finding on #2513). Fixed by teaching the shared scan
  (`param_helpers::attr_or_cfg_attr_matches_any`, now used by both
  `static_route.rs` and `ws.rs`) to also look inside a `cfg_attr`'s own
  argument list for a wrapped guard attribute.

- **macros: `#[static_get]`/`#[ws]` also missed a guard attribute imported
  under an alias (e.g. `use ::autumn_web::secured as auth;` then
  `#[auth("admin")]`):** a proc-macro attribute is invoked on raw syntax
  before the compiler resolves imports, so there is no API for a proc macro
  to ask "does this path actually name `autumn_web::secured`" — an aliased
  guard's spelling is fundamentally invisible to the name-based scan the
  ninth finding's fix still relied on, however thorough (tenth Codex finding
  on #2513, and a materially different problem from the ninth: no name-based
  check, however exhaustive, can close this one). Fixed the only way a
  syntactic scan can be made sound against a rename it cannot see through:
  `#[static_get]`/`#[ws]` now unconditionally leave a marker const in the
  body of every handler they accept
  (`param_helpers::STATIC_ROUTE_HANDLER_MARKER`/`WS_HANDLER_MARKER`,
  mirroring the `__AUTUMN_STEP_UP_MAX_AGE`/`__AUTUMN_THROTTLE_ROUTE_ID`
  body-marker-const technique already used to communicate across a macro
  expansion boundary elsewhere in this crate), and each of
  `secured_macro`/`step_up_macro`/`throttle_macro`/`authorize_macro` now
  checks for it (`param_helpers::reject_if_incompatible_route_marker`)
  immediately after parsing its own input — before doing any of its own
  work — regardless of what name or alias the compiler invoked it under.
  Covered by five new tests (one per guard, plus `#[ws]`) that construct the
  exact scenario an alias produces: accept a still-unrecognized attribute
  through `#[static_get]`/`#[ws]` first, then invoke the guard's macro
  function directly on the result, proving the rejection fires without
  ever relying on the guard's surface name.

- **macros: the tenth finding's marker could still be missed when another
  attribute macro sat between `#[static_get]`/`#[ws]` and the aliased
  guard:** `has_body_const_marker` only scanned a handler's top-level body
  statements, but `#[cached]` — a real, already-shipped macro in this crate
  — re-homes a handler's entire original body one level deeper, inside a
  `(|| async move { … })().await` closure IIFE (`cached_macro`'s
  `compute`). `#[static_get] #[cached] #[auth("admin")]` (`auth` an alias
  for `secured`) would leave the marker buried inside that IIFE, invisible
  to the flat scan, silently reopening the exact cache-bypass hole this
  whole rejection exists to close (eleventh Codex finding on #2513).
  `edge::stmts_have_marker` already solves this same "wrapper shape buries
  a marker" problem for `#[edge]` — by recursing into any expression an
  earlier guard's rewrite (or `#[cached]`'s IIFE, once taught the shape)
  might hide a marker inside — via the shared
  `idempotency_guard::expr_nested_async_body` helper, so `#[static_get]`'s
  new marker check needed the same descent, not a special case of its own.
  Taught `expr_nested_async_body` to also unwrap a zero-argument closure
  call (`(|| async move { … })()`, `#[cached]`'s exact shape), and switched
  `param_helpers::has_body_const_marker` to delegate to
  `edge::stmts_have_marker` instead of its own flat scan, so every consumer
  of the marker-const technique benefits uniformly. Covered by a new test
  that runs a handler through `#[static_get]` then `#[cached]` before the
  aliased guard, confirmed red (the marker present but buried, no compile
  error) before the fix and green after.
- **Database pool refusals echoed credentials into the boot log (issue
  #1905):** the pool's boot-time refusals name the offending target so the
  message is actionable, and did so verbatim. `setup_database` surfaces
  `PoolError` as `"Failed to create database pool: {e}"` and the run path logs
  it through `tracing::error!`, so under `log.format = "json"` a password
  reached the structured log stream — one line after `format_config_summary`
  had masked the same URL. Every refusal in the pool module now routes its
  target through a redactor: userinfo passwords are replaced, and for a
  Postgres target or one naming no backend at all the query string goes too,
  since `?password=`, `?sslpassword=` and `?api_key=` are real spellings that
  a userinfo check never sees and an unclassifiable target offers no way to
  enumerate. A SQLite target passes through whole — a local file URI carries
  no credentials, and its query string (`mode=ro`, `mode=memory`,
  `cache=shared`) is the diagnostic detail the replica and read-only messages
  exist to report. The libpq keyword/value form (`host=db user=app
  password=hunter2`) is rebuilt from an allowlist of the keys that identify
  the target (`host`, `hostaddr`, `port`, `dbname`, `user`) — an allowlist
  rather than a `password`/`sslpassword` denylist, so a key this code has
  never heard of cannot default to being printed. Anything the redactor cannot
  classify — a malformed keyword/value string among them — is masked outright:
  the default is to hide, and the one exception is a single path-shaped token
  carrying no `=`, `@`, `?` or whitespace, so a bare filesystem path stays
  legible where naming it is the whole value of the message.

- **SQLite pool construction accepted a target that names no backend (issue
  #1905):** under `--features sqlite`, `build_sqlite_pool` guarded only
  against a Postgres target. Everything else fell through to
  `normalize_sqlite_target`, which strips the `sqlite:` / `sqlite://` schemes
  and passes anything it does not recognize along verbatim — as a
  **filename** — so a `mysql://…` URL, a typo of the scheme
  (`sqllite:///app.db`) or a bare filesystem path (`/var/lib/app.db`) built a
  pool over a junk file rather than refusing. On the ordinary boot path
  `DatabaseConfig::validate` already rejects those shapes, so this was not a
  live misboot for an app configured through `autumn.toml`; the gap was in
  the **public** `create_pool` / `create_topology` / `create_shard_topology`
  API, which a programmatically-built `DatabaseConfig` or a custom
  `DatabasePoolProvider` reaches without that screen. The pool is the layer
  that decides what the string *means*, so it now decides the same way
  everything else does: it accepts only a target `DatabaseBackend::detect`
  classifies as SQLite — the same predicate `autumn doctor`, the generator's
  DDL mapping and `autumn migrate` use — and its refusal names both the
  offending target and the accepted spellings (`sqlite:<path>`,
  `sqlite://<path>`, `file:<path>`). This makes the runtime match the
  published contract that a bare filesystem path is not a recognized SQLite
  target.

- **The SQLite backend's own unit tests never ran in CI (issue #1905):** the
  `sqlite-runtime` job builds the crate, lints it with `clippy --lib`, then
  runs `cargo test … --test <name>`. `--test` selects test *targets*, so the
  ~33 `#[cfg(feature = "sqlite")]` unit tests inside the crate — pool sizing,
  the replica rejections, in-memory/read-only target classification, provider
  dispatch, the sharding refusals, the sim substrate — were run by nothing.
  Nor were they even *compiled*: `clippy --lib` without `--all-targets` does
  not build `#[cfg(test)]` code, so the crate's unit-test tree had never been
  type-checked under the backend flip. Under that blind spot the ungated pool
  and topology tests sitting beside them had rotted into 10 hard failures
  against it, because their fixtures hard-code `postgres://` URLs the SQLite
  pool refuses. Those
  fixtures are now backend-parametric (the mechanics they assert — `max_size`,
  the connect-timeout → wait/create mapping, replica retention, the read-pool
  fallback — are backend-independent, so they now run on *both* backends
  rather than being silenced on one), and the job gained a
  `--features sqlite --lib` step covering every module that carries
  SQLite-specific logic. The `sqlite_json_field_conversion` `[[test]]` target
  (#1341), declared in `autumn/Cargo.toml` and named by no CI lane, is now
  run by the integration step alongside its siblings.

- **ci: `live_upgrade` no longer fails because the hot-upgrade example inherits
  the dev profile's 1-second drain budget:** the test asserts the production
  guarantee that a predecessor drains and exits 0 after handing over, but ran
  under the `dev` profile, which shortens `shutdown_timeout_secs` from the 30s
  default to 1s so Ctrl-C is snappy while developing. The load loop keeps
  driving traffic for 3.5s past the cutover, so that budget expires while
  requests are still arriving and whatever is in flight is aborted
  (`exit_code: 1`) — nothing wrong with the upgrade path, just a budget shorter
  than the load window. The test now pins `AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS`
  to the production default, exactly as it already pins the prestop grace.
  Reproduced locally under CPU oversubscription (which also disproved the
  intuitive "slow cutover" explanation — it reproduces with the cutover
  completing in 135 ms), and verified non-hiding: under the same load the
  connection assertions still fired on their own, so the drain budget was not
  what was holding them up. Independent of, and complementary to, the
  refused/reset split below — that one classifies *which* connection failures
  count, this one stops the predecessor being killed mid-drain.
  [no-plugin] — test-only; no API or behaviour change. (#1747, #2372, #2462)
- **`autumn new`'s starter template shipped three cookie-consent routes that
  failed `autumn routes audit` (issue #1214 follow-up):** the cookie-consent
  banner feature added `POST /consent/accept`, `POST /consent/reject`, and
  `GET /consent/manage` to `main.rs.tmpl`, but — unlike every other starter
  handler — never marked them `#[public]`, so a fresh `autumn new` app failed
  its own generated CI's routes-audit gate before a single line of app code
  was written. Invisible until now: `scaffolded_app_passes_routes_audit_gate`
  (added for #2154 specifically to catch this class of regression) was one of
  the `cli_tests` tests #1945 revived — it had never run in CI before. Added
  `#[public]` to all three handlers, matching the pattern documented right
  above them in the template. [no-plugin] — restores previously-documented
  behavior; no new or changed API.
- **`route_macro` lost a guarded handler's OpenAPI response schema under
  `#[throttle]`/`#[step_up]` (issue #2516):** #2488 moved the
  `#[throttle]`/`#[step_up]` auth/rate-limit checks out of the handler body
  and into a sibling `FromRequestParts` gate, but stopped emitting the
  guard's marker const (`__AUTUMN_THROTTLE_ROUTE_ID` /
  `__AUTUMN_STEP_UP_MAX_AGE`) into the handler body — `#[secured]` still
  does. `api_doc::infer_response_body`'s recovery of a guarded handler's
  pre-rewrite return type (#1677/#2484) requires that marker to trust the
  `__autumn_inner` binding it reads the type back from, so a
  `#[throttle]`/`#[step_up]`-guarded route written above `#[post]`/etc.
  silently lost its documented `Json<T>` response the moment #2488 merged.
  Both macros now emit their marker const into the handler body a second
  time (unused there, `#[allow(dead_code)]`), mirroring the pattern
  `secured_macro` already used. Caught as a fully red `autumn-macros` test
  suite on `trunk-dev` itself (4 `route::tests::route_macro_infers_response_schema_*`
  tests), not by a diff — two individually-green PRs (#2484, #2488) composed
  into a broken `trunk-dev`. [no-plugin] — restores previously-documented
  behavior; no new or changed API.

- **hot-upgrade example: `live_upgrade` test no longer conflates a mid-flight
  reset with a refused connection, and no longer lets an unbounded number of
  resets pass silently (issue #2462):**
  `upgrades_in_place_under_load_without_dropping_a_connection_or_the_state`
  counted every request failure — `ECONNREFUSED` (nothing listening, the
  actual zero-downtime violation) and `ECONNRESET`/`ECONNABORTED` (a
  connection torn down mid-flight) — into one `connect_errors` counter, then
  asserted zero on it with a message claiming *"no connection may be
  refused"*. That failed once, intermittently, on `Test (macos-latest)` with
  two `ECONNRESET`s, and the message actively misled: it named refusal when
  the actual cause was a reset. Refused and reset are now classified
  separately (`is_connection_refused`/`is_mid_flight_reset`); a mid-flight
  reset gets one retry (`with_reset_retry`, after a short 20ms backoff so it
  lands after the handover settles rather than in the same window that reset
  the first attempt), while a refused connection is a hard, zero-tolerance
  failure on the first attempt — never retried, since there is nothing to
  wait out. A retried success's `latency` is overwritten to span the whole
  attempt (failed try + backoff + retry), not just the retry's own timing, so
  a genuine cutover latency spike can't hide behind a cheap retry.
  Autumn's in-place upgrade hands the successor the *same* listening socket
  by duplicating its fd across the `exec` (`HandoffSocket::from_listener`),
  not by binding a second `SO_REUSEPORT` socket, so — unlike this issue's own
  initial theory — there is no accept-queue race between two listeners to
  blame a reset on; the root cause of the one observed instance remains
  unconfirmed. Retrying past a reset is therefore now bounded
  (`MAX_TOLERATED_RESETS`, asserted): an isolated anomaly doesn't fail the
  run, but a systemic source of resets — real evidence of a defect in the
  handoff or drain path — still does, rather than being silently retried
  away every time. The failure message and a `println!` summary say which of
  refused/hard-failed/retried happened.
  `with_reset_retry`'s branching (retry-then-succeed, a second reset still
  fails, a refusal is never retried, an unrelated error kind is never
  retried, and the spanning-latency behavior) is unit-tested directly against
  fake attempt closures, independent of the real network/signal machinery the
  rest of the test drives.

- **🔒 `autumn generate auth`: a concurrent successful login could be silently
  re-locked by a racing failed attempt (issue #2500):** the generated
  `login` handler (and the duplicated `reauth` step-up block) counted a
  wrong-password attempt with an atomic `failed_attempts + 1` `UPDATE` and
  then, once the new count crossed `[auth.lockout].threshold`, stamped
  `locked_at` with a *second*, unconditional `UPDATE ... SET locked_at =
  now() WHERE id = ?` — gated only by a stale in-memory `current_locked_at`
  value read at the top of the request, never re-checked against the
  database. If a concurrent request with the *correct* password committed
  its own "successful login resets the counter" `UPDATE`
  (`failed_attempts = 0, locked_at = NULL`) in the gap between the failed
  request's two statements, the unconditional lock stamp reapplied on top
  of that reset — silently re-locking an account for the full `cooloff_secs`
  window (default 15 minutes) even though it had *already* logged in
  successfully (303 redirect and session cookie already issued). Reproduced
  12/100 (and 7/60, 3/100 on reruns) with a genuinely concurrent
  wrong-password/correct-password pair against a fresh `autumn generate
  auth` scaffold.
  Fix: the lock-stamp `UPDATE` in both handlers now filters on the row's
  *current* `failed_attempts` (`>= threshold`) and `locked_at`
  (`IS NULL`) at write time instead of trusting the in-memory read, so a
  concurrent successful reset makes the stamp a no-op rather than
  clobbering it; the `account_locked` telemetry event is now gated on the
  stamp actually having affected a row, so a losing race is never logged
  as a lock. New generator meta-tests (`autumn-cli/src/generate/auth.rs`)
  assert both guards are present in the generated source, and
  `autumn/tests/integration/auth_lockout_race.rs` (Postgres testcontainer)
  reproduces the exact bad interleaving deterministically against a real
  database — showing the pre-fix pattern re-locks the account and the fixed
  pattern doesn't, plus the mirror-image ordering where a lock that
  genuinely wins the race correctly rejects the concurrent login instead of
  silently granting a session.
- **🧭 Wayfinder: keyboard bypass-blocks link added to 6 supported example
  apps (a11y `bypass` Serious 7/8 → 0/8; `landmark-one-main` Moderate 1/8 → 0/8):**
  `autumn check --a11y` — the framework's own WCAG audit, run against each
  layout's rendered shell — found `todo-app`, `blog`, `bookmarks`,
  `bookmarks-distributed`, `wiki`, `saas`, and `teams` (7 of the 8 `supported`-tier
  example apps with an HTML UI; `reddit-clone` already had it) missing the
  skip-to-content link that `autumn new`'s own scaffold (`autumn-cli/src/templates/main.rs.tmpl`)
  ships by default. A keyboard-only user visiting any of these — including
  `saas`/`teams`, whose nav is the login/signup entry point — must tab through
  every nav link before reaching the page's actual content on **every single
  page load**, with no way to jump past it (WCAG 2.4.1 Bypass Blocks). `todo-app`
  additionally had no `<main>` landmark at all, so a screen-reader user's
  "jump to main content" shortcut had nothing to land on.
  Fix: a visually-hidden, focus-revealed `<a href="#main-content">` as the
  first element inside `<body>` on the 6 apps whose layout has a nav/header
  preceding the content, plus a `<main id="main-content">` landmark on every
  affected layout including `todo-app` — the exact skip-link pattern
  `examples/reddit-clone` already used and the scaffold template establishes,
  so no new CSS or dependency is introduced. `todo-app`'s page has no
  nav/header at all (content is the first thing in `<body>`), so it gets only
  the `<main>` landmark — a skip link there would add a tab stop ahead of the
  first form control while bypassing nothing, the same reason
  `examples/media-room` (also audited, also nav-less) was left unchanged.
  `examples/blog` is i18n-aware (`/es/...` routes via `t!`), so its skip
  link's label is a new `layout.skip_to_content` Fluent key translated in
  both `i18n/en.ftl` and `i18n/es.ftl`, not a hardcoded English string,
  matching the rest of that layout's chrome.

  `autumn check --a11y`'s `bypass` rule (`autumn-cli/src/check.rs`) itself had
  a false positive this fix exposed: it unconditionally required a skip link
  as the page's first `<a>`, so `todo-app`'s post-fix shell — `<main>`
  immediately inside `<body>`, its only link in the footer, after `<main>` —
  still reported Serious `bypass`, even though there is no nav/header for a
  skip link to bypass there. The rule now skips the check when nothing
  (no nav, header, or link) precedes `<main>`, matching the exact reasoning
  already applied to `todo-app`/`media-room` above; it still fires whenever
  content — `<nav>`-wrapped or not — precedes `<main>` without a skip link
  (both pre-existing regression tests plus two new ones cover this).
- **🛣️ Onramp: `autumn setup` retries a dropped Tailwind CSS download instead
  of failing the whole quickstart [no-plugin]:** `autumn setup` — the second
  documented command in the README quickstart, right after `autumn new` — did
  a single unretried `GET` for both the Tailwind CSS checksums manifest and
  the ~10MB platform binary (`autumn-cli/src/http.rs`); any transient
  transport hiccup (a truncated body, a dropped connection) aborted the whole
  command with a bare `Error: download failed: error decoding response body`
  and no second chance. This is not hypothetical: 2 of the last 3
  `quickstart-gate.yml` runs against published crates.io on 2026-09-03 (runs
  33784307158 at 17:23 UTC and 33805758766 at 21:02 UTC) failed at exactly
  this step with exactly this error, with the passing run in between
  (33797353571, 19:35 UTC) at the same commit — confirming the failure is
  transient CDN flakiness, not a real break, and that a real fraction of
  fresh `autumn setup` runs hit it. `fetch_bytes` and the new `fetch_text`
  (both in `autumn-cli/src/http.rs`, shared by `autumn setup` and `autumn
  assets`) now retry up to 3 times with a 2s backoff on any non-HTTP-status
  error (a definitive 404/5xx is not retried — retrying it would just waste
  the user's time); the retry/backoff bookkeeping is exercised by 4 unit
  tests against fake failing/succeeding closures, no real networking
  involved. No public API changed — `fetch_bytes`'s signature and error type
  are unchanged, `fetch_text` is a new addition. No plugin-facing surface;
  this is a CLI robustness fix, not new framework surface.

- **`--counter-cache` scaffolds compiled clean now (#2431):** `autumn generate
  scaffold Comment ... --belongs-to Post --counter-cache` — the documented,
  only way to use the flag — generated a child model that failed `cargo check`
  outright, every time. `#[belongs_to(Post, counter_cache)]` landed ABOVE
  `#[autumn_web::model]` instead of below it: that attribute is a helper only
  `#[model]`'s own expansion understands, so rustc rejected it as an unknown
  standalone macro (`cannot find attribute belongs_to in this scope`) whenever
  the generated file had so much as a blank line before `#[model]` — which
  every real scaffold does (the `use crate::schema::...;` line that always
  precedes it). Fixing the position surfaced a second, previously-unreachable
  gap: the eager-loading codegen a `#[belongs_to]` attribute drives references
  the parent's type and its Diesel schema module directly, and neither was
  ever imported into the child's model file — `--counter-cache` was the first
  feature to put an attribute-driven association on a generated model at all.
  Both are now added automatically (`use crate::schema::{parents};` and `use
  crate::models::{parent}::{Parent};`, alongside the child's own), and a new
  `cargo check`-backed test (`generated_counter_cache_scaffold_cargo_checks`)
  proves the whole scaffold compiles — the gap the original report called out:
  no test had ever compiled this flag's output before.

- **A sandboxed plugin can no longer abort the application at boot:** the
  duplicate-route preflight skips `nest` mounts because axum exposes no way to
  enumerate a nested router — but a sandboxed plugin's manifest *is* its route
  table, and `Router::nest` panics with `Overlapping method route` when a
  declared path is one the host already serves. An untrusted artifact declaring
  a plausible prefix (`/admin`, `/status`, `/api`) could therefore take down
  every route in the application, not just its own — containment failing open
  for exactly the input class the sandbox lane exists to distrust. Routes
  declared via `AppBuilder::declare_plugin_routes` are now checked against the
  application's own routes through the same `matchit` oracle axum routes
  through, so a collision — exact path, shape clash (`/hello/{id}` against a
  declared `/hello/{slug}`), or catch-all — is a `RouterBuildError` naming the
  plugin and the contested path, raised before anything mounts. Paths that axum
  accepts (disjoint siblings under a shared prefix, a route *at* the prefix, a
  GET and its implied HEAD) are unaffected. `TestApp` carries these
  declarations too, so a colliding plugin fails in tests rather than only in
  production. Framework-mounted paths (probes, actuator, htmx assets, mail
  previews, the story gallery, the tracked-job status route) are covered as
  well: they are mounted outside the user route list, so a manifest declaring
  `GET /health` would otherwise still have panicked. A framework path is
  *refused* rather than yielded — a user route at a probe path legitimately
  takes it over, but silently handing an unaudited artifact the endpoint
  orchestrators read to decide whether the process is alive is worse than a
  loud refusal. Only `GET` is refused there, because only `GET` clashes; a
  declared `HEAD` or `POST` merges into the same `MethodRouter` cleanly. The
  framework namespaces `/static` and `/_autumn` are reserved wholesale, for
  every method: paths under them are not enumerable route-by-route (`ServeDir`
  serves whatever is on disk), and a declared sub-path there does not even
  panic — it mounts and *shadows* the framework, so an artifact declaring
  `/static/app.js` would serve script from the host's own origin. Matching is
  on segment boundaries, so `/staticky` is unaffected. Probe paths are claimed
  only when `health.enabled`, matching the mount, so a plugin is never refused
  over a collision that cannot happen. Framework paths are compared through the
  same matchit oracle as user routes, not by string equality, so a framework
  template carrying a capture (`/_stories/{slug}`, or an operator-configured
  probe or actuator path) is not an exact-string miss and a startup panic. The
  dev inspector's detail route (`{inspector_path}/requests/{id}`) is claimed
  alongside its index — both now derive from `inspector_endpoint_paths`, so the
  claim set cannot drift from what the router actually mounts.

- **`#[secured]`/`#[step_up]`/`#[authorize]`/`#[throttle]` no longer drop a
  route's OpenAPI response schema when written above the route attribute
  (#1677):** all four body guards rewrite a handler's return type to
  `Response` when they expand, so when one was written *above* `#[get]`/
  `#[post]`/etc. it expanded first and the route macro's `infer_response_body`
  read back `Response` instead of the handler's real `Json<T>` — silently
  dropping the response schema from the generated OpenAPI document (throttling
  itself, including idempotency-replay accounting, was unaffected in either
  ordering). Each guard already binds the pre-rewrite type as
  `let __autumn_inner: T = …` around the guarded body; the route macro now
  recovers the original type from that binding — matched by its exact
  structural shape and the presence of the guard's own marker const earlier
  in the same block, not merely the binding's name or position, so neither
  an unrelated handler-local nor a coincidentally-shaped fragment of the
  handler's own body is ever mistaken for a real guard's binding — recursing
  to the innermost binding when guards stack, instead of trusting
  `sig.output` alone. The generated schema no longer depends on attribute
  order. The
  previously-recommended method-attribute-outermost workaround, documented
  on `#[throttle]`'s rustdoc and in `docs/guide/rate-limiting.md`, is no
  longer necessary.

  This is a macro/metadata-only change — no authorization, rate-limiting, or
  idempotency-replay runtime behavior differs in either attribute ordering.
  `ApiDoc::response` does have one existing runtime reader, `mount_mcp`'s MCP
  tool-catalog eligibility gate: a guard-above-route JSON handler explicitly
  opted in with `#[api_doc(mcp)]` was previously excluded from `tools/list`
  as "no response schema", and is now correctly listed, matching the
  developer's existing opt-in — every MCP call still dispatches through the
  same authenticated handler pipeline, so this closes an availability gap
  rather than changing what a call is authorized to do.

- **Punctuation- and emoji-only titles no longer slip past the validator that
  exists to stop them (#2424):** `examples/reddit-clone` rejects a post title
  like `***`, `!!!???...:::` or `🎉🔥💯` with "Title must contain at least one
  letter or number" — as its own doc comment always claimed it did. The check
  had gone dead: it asked `slugify(value).is_empty()`, and `slugify` had since
  been given a stable non-empty fallback token, so the condition could never
  be true again. A content-free title was silently accepted and published under
  a hash-looking URL (`/r/rust/comments/35/n1a3b8617ffb1dc4d`) with no feedback
  to its author. Two sibling checks written the same way — the community name
  in `subreddits::create` and the per-name skip in the post-tag parser — were
  equally unreachable and are fixed with them.

  The framework half is a new **`autumn_web::contains_letter_or_number`**
  (described under *Added* above). Applying it narrows nothing that was
  working: the dead check accepted everything, so a `"日本語"` or `"Привет"`
  title was already accepted and stays accepted, taking `slugify`'s hash
  fallback for its URL segment — exactly what that fallback is for. The rule
  itself changes the outcome only for input carrying no letter or number at
  all.

  The same rule is now also applied in `PostHooks` and a new `SubredditHooks`,
  because the generated `/api/posts` and `/api/subreddits` routes run the
  model's `#[validate]` attributes and the mutation hooks — never the
  route-local validator — so `{"title": "***"}` could reach the database
  through the API even with the form path fixed. A model-level
  `#[validate(custom(...))]` would not have been enough: `#[model]`
  deliberately drops `custom` from the `UpdateModel` PATCH path (`Patch<T>`
  implements only the declarative per-field traits), so it would have covered
  an API create and left an API rename to `"***"` open. A hook sees the merged
  model on both.

  Two adjacent corrections in the same example, both surfaced while fixing the
  above: the community-name length rule counted **bytes** (`str::len`) while
  telling the user it counted characters, so an 11-character Japanese name was
  rejected as over 32 and a 1-character one passed a rule requiring 2 — it now
  counts characters, matching the post title's `validate(length(...))`. And
  saving tags now reports how many names were ignored instead of returning
  fewer tags than the author typed under a bare "Tags updated."
- **🧭 Wayfinder: text-safe warning/success colors in the admin plugin
  (WCAG contrast 3.19:1 / 3.77:1 → 4.5:1+):** the Runtime Config page's
  "overridden" status badge (`--warning` #d97706 on `--surface`, every
  deployment's config page) and every model list page's boolean-field ✓
  checkmark (`--success` #059669 on `--surface`, rendered for every boolean
  column in every row) used their raw semantic color token directly as
  small/normal foreground text — tokens calibrated for the 3:1 large-text/
  border/icon uses they already had, not WCAG AA's 4.5:1 normal-text
  threshold. New `--warning-text` (#92400e) and `--success-text` (#065f46)
  tokens, matching the framework's existing flash-message foreground shades,
  fix both sites without introducing a new color.
- **Dependent cascades are documented where the guide already pointed, and the
  `through =` rejection points at the offending key (#1702):** the repositories
  guide had no dependent-cascade section at all, even though the counter-cache
  guide sent readers there for it, and the macro-transparency guide still
  described the cascade as **single-level with grandchildren unhandled** and as
  `delete_by_id`-only — both untrue since the cascade became recursive (#1739)
  and bulk-aware (#1740). `docs/guide/repositories.md` now carries the canonical
  treatment: both declaration sites (`#[has_many(Child, dependent = <action>)]`
  on the model, `dependent(PgChildRepository, fk = "...", on_delete = ...)` on
  the repository as the escape hatch for children outside the
  `Pg{Child}Repository` convention), the precedence rule between them, the four
  actions, the transactional/ordering guarantees, and the rejected combinations;
  `docs/guide/macro-transparency.md` is corrected and links to it. Alongside it,
  a `dependent`/`on_delete` on a `through = <join_table>` association now spans
  the `dependent` key itself rather than the association's target ident, so the
  caret lands on the thing the error tells you to remove. The model-declared
  `destroy` cascade on `#[has_one]` is now proven end to end, the precedence
  between the two declaration sites is proven behaviourally, and the three directed
  compile errors (`dependent` on `#[belongs_to]`, on a `through =` association,
  and an unknown action) are pinned by trybuild fixtures.

- **A NUL byte in a form field is a validation error, not a 500 (#2423):** a
  Postgres `TEXT`/`VARCHAR` column cannot hold `0x00`, but nothing between the
  form body and the `INSERT` could say so. An embedded NUL — which a real user
  can produce by pasting from a binary source or through an input-method glitch,
  not only deliberately — decoded cleanly, satisfied every `#[validate(...)]`
  rule (the value is a perfectly good Rust `String`), and failed only when the
  driver handed the byte to the server: `invalid byte sequence for encoding
  "UTF8": 0x00`, surfacing as an uncaught `AutumnError` and a `500`. By the
  framework's own error-class convention that reads as a server bug, when it is
  malformed client input.

  `ChangesetForm` and `NestedChangesetForm` now sweep every submitted **text**
  value before it is deserialized — the only point where the offending field is
  still identifiable by name — and record
  `autumn_web::form::NUL_CHARACTER_FIELD_ERROR` ("Cannot contain the NUL
  character (0x00)") against the field or child subfield that carried one.
  Handlers need no change: it is an ordinary field error, so the form
  re-renders inline through the existing `ChangesetForm` round-trip. The
  retained value is the author's text minus the byte, so the re-rendered form
  keeps their work, never echoes a raw `0x00` into the HTML, and their next
  submission succeeds. Framework plumbing fields are exempt under both their
  default and configured names — `_csrf`, `_submit_token`, `_method`, and a
  nested row's `_destroy` — because no template renders an error against one,
  and cleaning `_destroy` would flip a falsy marker truthy. File parts of a
  multipart body are untouched.

  For the paths neither round-trip extractor sees — a JSON API body, a
  hand-written query, a background job, an `extract::Form` or `Valid<T>`
  extraction — the Postgres rejection is now classified instead of
  blanket-500'd: the `AutumnError` carries `422 Unprocessable Entity`, and
  `autumn_web::error::is_nul_byte_violation` recognizes it (walking the
  `source()` chain) for handlers that want to fold it back into a form the way
  `unique_violation_field` is used for a uniqueness clash. Classification is by
  server message rather than SQLSTATE because `diesel-async` maps `22021` to
  `DatabaseErrorKind::Unknown` and diesel's `DatabaseErrorInformation` exposes
  no code; the match is anchored on the trailing `: 0x00` and the encoding name,
  so a message that merely *contains* `0x00` — including one echoing text a
  client submitted — keeps its 500 rather than being relabelled as the client's
  fault. The classified error is also re-wrapped rather than merely restatused,
  because the 422 error page renders the message where the 500 page redacts it:
  the client sees a fixed sentence and the raw server message stays in the
  `source()` chain for logs.

  New: `Changeset::add_error`, for folding a post-decode failure back into a
  form round-trip; `autumn_web::normalize::strip_nul` and
  `#[normalize(strip_nul)]`, for columns where dropping the byte silently beats
  refusing the write (insert and hooked-update paths only — the documented
  normalize-vs-persist asymmetry still applies). See `docs/guide/forms.md` —
  "Unstorable bytes: NUL", which also records what the backstop does not cover:
  a 422 with no field name, and `JSONB`'s different SQLSTATE (`22P05`).

- **ACME config: `autumn doctor` and the runtime now reject the same two
  spellings (#1874):** two low-severity parity gaps let `autumn doctor
  --strict` bless an `autumn.toml` the server refuses to boot on. A
  **non-integer `renew_before_days`** — a quoted `renew_before_days = "30"`, a
  float, or a bool — was read by doctor's `as_integer()` chain as *absent* and
  silently defaulted to 30, so the check passed while the runtime's typed
  deserialization rejects the file at boot. Doctor now records the value the
  operator actually wrote and reports an `acme_config` **Fail** naming it,
  exactly as it already did for `http_challenge_port` and `directory`. (A
  negative or out-of-`u32`-range integer already failed, having been clamped to
  `u32::MAX` and caught by the `>= 90` rule; it now fails with a message about
  the value rather than about the renewal window.) A
  **whitespace-padded domain** (`domains = [" app.example.com "]`) passed
  `AcmeConfig::validate()`, which trims only for its blank and wildcard checks
  but stores the entry untrimmed — and the untrimmed string is what becomes the
  certificate's SAN and the ACME order's `Identifier::Dns`, so the padded name
  was requested as-is and failed mid-issuance with an opaque CA error. Both
  `validate()` and doctor now reject it at startup with a message naming the
  entry, its index, and the trimmed spelling to use. Neither gap was a security,
  denial-of-service, or CA-rate-limit issue: each was already caught fail-fast
  at boot or at first issuance, just later and less legibly than it should have
  been.

- **CI now rejects colliding migration versions:** app, framework and plugin
  migrations are applied into one shared version space — diesel keys
  `__diesel_schema_migrations` on the 14-digit version and
  `autumn_migration_checksums` makes it a `PRIMARY KEY` — so two migrations
  claiming one version are not two migrations: the loser silently never runs.
  Hand-written day-granularity names (`YYYYMMDD000000`) collide by
  construction, because every author who types a date pads the same six zeros.
  The damage was already in the tree: `examples/reddit-clone` carries
  `20260513000001` and `20260702000001`, hand-bumped by one off framework
  versions, and `00000000000000` was shared by the framework, the starters, the
  benchmark app and eight examples. A new gate
  (`scripts/check-migration-versions.sh`, run in the `Migration guide coverage`
  job) fails a migration whose time component is `000000`, whose digits are not
  a real UTC timestamp (`20260530300000` has hour 30), whose name is not
  `<14 digits>_<snake_case>`, or whose version is already claimed. The
  generators already did the right thing — `autumn generate migration` and
  `autumn schema diff --write-migration` mint a full `YYYYMMDDHHMMSS` from the
  clock — so this closes the hand-created-directory bypass rather than adding a
  new convention. Pre-existing offenders are grandfathered in
  `scripts/migration-version-baseline.txt` and deliberately **not** renamed:
  they have already been applied to real databases, and renaming one makes the
  framework consider it unapplied and run it again.
- **Failure capsules: credential components no longer miss the spelling the
  handler holds (#2212):** `record_credential_components` records the secret
  *inside* a masked header — the token after an auth scheme, each auth-param
  value, each cookie value — so the echo set matches what a handler actually
  extracted rather than only the whole header line. Two spellings escaped it.

  The parser began by requiring the whole header value to be UTF-8, so a
  single `obs-text` byte — legal inside a `quoted-string`, and reachable with
  a perfectly valid header — skipped **every** component, including parameters
  that were plain ASCII and independently parseable. A byte-oriented `Digest`
  handler extracted `response="deadbeef"` from
  `Digest username="<0xff>alice", response="deadbeef"`, echoed it into an
  error and bound it, and nothing in the echo set matched. The parser now
  works on bytes throughout: the scheme split, `is_auth_scheme` and
  `is_token68`, and the quoted-string walk in `split_auth_params` /
  `unquote_auth_param`. Same class as the `Basic` case fixed earlier, where
  requiring the decoded `user:password` to be UTF-8 discarded an ASCII
  password because of a byte in the username beside it.

  A `Cookie` value was recorded exactly as it arrived, so `session=abc%2Fdef`
  contributed `abc%2Fdef` and nothing else. Percent-encoding cookie values is
  a common convention that Autumn itself follows — the `autumn_time_zone`
  cookie is percent-decoded before use — so an application doing the same
  holds `abc/def`, which matched neither the whole header nor the recorded
  component. Cookie values now join the echo set under both spellings, and
  stay whole-token-only so a `theme=dark` cookie still cannot shred *darkness*
  in an unrelated failure. Unlike the form and query values
  `mask_raw_urlencoded` records both spellings for, a cookie value is not
  form-encoded: `+` stays a `+` rather than folding to a space, matching
  Autumn's own cookie decoder.

  The decoded spelling — an inference about what a handler holds, rather than
  something the request carried — is only recorded once it is at least four
  bytes and not all whitespace. `%2F`, `%3D`, `%30` and `%20` decode to `/`,
  `=`, `0` and a space, and a one-byte needle masked as a whole token would
  rewrite `failed at /`, `x = y` and `status 0` in the outcome while blanking
  any bind equal to it, which drops that column from replay's comparison. The
  spelling that actually arrived is still recorded however short, exactly as
  before.
- **`rediss://` no longer panics at startup, in any Redis-backed subsystem
  (#2172):** `redis`'s `tokio-rustls-comp` builds its TLS `ClientConfig`
  through `rustls::ClientConfig::builder()`, which *panics* rather than
  erroring when no process-level `CryptoProvider` is installed — and rustls
  can only resolve one implicitly while exactly one of its `ring`/`aws-lc-rs`
  features is on, which Cargo's whole-graph feature unification can break from
  any dependency (`telemetry-otlp` alone is enough). Only `autumn-cache-redis`
  guarded against this; sessions, channels, the Redis job queue, job tracking,
  idempotency, webhook replay and Redis rate limiting each opened their own
  unguarded client. Since the `azure-container-apps` release target provisions
  a Redis Cache with `non_ssl_port_enabled = false` — it can only ever hand the
  app a `rediss://` URL — pointing any of those subsystems at it crashed the
  app on boot.

  Every Redis client the framework opens now goes through
  `autumn_web::redis_tls::open_client`, which installs `ring` once,
  idempotently, and only when the URL is a TLS one — decided by asking the
  `redis` crate to parse it and checking whether it resolved to a TLS address,
  rather than by keeping a second copy of that crate's scheme table. So
  `rediss://`, Valkey's `valkeys://` and every case variant the URL parser
  accepts are covered with nothing to re-check on a dependency bump, and
  prefix lookalikes (`redisstore://`), `unix://` sockets and unparseable input
  are classified exactly as the connector will classify them. A plaintext
  `redis://` URL deliberately does **not** claim the process-wide default, so
  it cannot pre-empt an application that installs `aws-lc-rs` for something
  else, and an already-installed provider is always kept rather than replaced.
  `autumn-cache-redis` now delegates to the same guard instead of carrying its
  own copy, the `reddit-clone` example is wired through it too (examples get
  copied), and a source scan in each of the three crates keeps a future
  subsystem from re-opening the hole with a bare `redis::Client::open`.

  Redis URLs are no longer logged verbatim. A managed Redis carries its access
  key *inside* the URL, and the rate-limit backend echoed the configured URL
  into a `WARN` on both of its fallback-to-memory paths — writing that key to
  whatever log sink the app ships to. `autumn_web::redis_tls::redact_url`
  masks the password, and deliberately over-redacts rather than under-redacts.
  Every input it sees on that path is malformed by definition, and malformed
  is exactly where a redactor gives up: an Azure access key is base64, whose
  alphabet includes `/`, and an un-encoded `/` ends the URL's authority early
  — which is both why the URL fails to parse and why the log line is reached.
  A mistyped scheme delimiter (`rediss:/:key@host`) leaves nothing to split
  on at all. Neither returns the input untouched. The invalid-URL branch now
  also logs no URL whatsoever, redacted or not: the `redis` error names the
  problem without echoing the value.

- **A container that terminates TLS itself is no longer permanently
  `unhealthy`:** the Dockerfile `autumn release init` generates hardcoded its
  `HEALTHCHECK` to `curl -f http://localhost:3000/health`, so an image whose app
  serves direct HTTPS (`[server.tls]`) failed every probe — Docker marked it
  unhealthy forever, and in the generated `docker-compose.yml` anything waiting
  on `condition: service_healthy` never started. The probe URL is now
  `${AUTUMN_HEALTHCHECK_URL:-http://localhost:3000/health}`, so the default is
  byte-for-byte today's plain-HTTP check and an HTTPS deployment sets that plus
  `AUTUMN_HEALTHCHECK_INSECURE=1`. The second variable is needed because the
  probe is a loopback call to the container's own listener while the
  certificate is issued to the app's public hostname, so it can never validate
  as `localhost`; it is an explicit opt-in rather than something inferred from
  the URL, because `user@host`, `#fragment` and lookalike hostnames all yield
  URLs that read as loopback but that curl resolves elsewhere. Unset — the
  default — the probe always verifies. Issue #1603.

- **CSV import row numbers are now the same for CRLF and LF files:**
  `autumn_web::data::csv::import_csv` reports a 1-based line number for every
  `CsvRowError`, but the underlying CSV parser's own counter runs exactly one
  behind on a CRLF file — the dialect Excel and most Windows tools write — so a
  row a spreadsheet shows on line 7 was blamed on line 6, while a byte-identical
  LF copy of the same file correctly said 7. An error report whose row numbers
  depend on the file's line endings is worse than no row number at all, so
  `import_csv` now calibrates against the header (whose true span is known: it
  is line 1) and shifts every reported position by the same amount, and it strips
  the parser's own uncalibrated line number out of the message text so a report
  never shows two different lines for one row. Multi-line quoted fields still
  push the rows after them down, in both dialects. The shift is measured once,
  so a file that *mixes* terminators still drifts — noted where the calibration
  lives. The header's true span is measured (a quoted header field may carry
  embedded newlines) rather than assumed to be one line, so the correction fixes
  the CRLF case without disturbing rows under a multi-line header.
- **`cargo test` on the workspace no longer aborts with a stack overflow:**
  clap's derive macro expands the whole `autumn` CLI — every subcommand, every
  argument — into a single `augment_args` function, and unoptimized that one
  frame is larger than the 2 MiB stack libtest gives a test thread. The first
  test to call `Cli::try_parse_from` therefore overflowed and took the process
  down with `SIGABRT`, stopping the suite before ~5,600 other `autumn-cli`
  tests ran. Release builds inline the frame away, which is why only `cargo
  test` was affected. A workspace `.cargo/config.toml` now sets
  `RUST_MIN_STACK`, so the fix travels with the code instead of living in a CI
  environment variable — the suite cannot pass on one machine and abort on
  another.

- **TLS-enabled migrations no longer panic when applied from an app's own
  async `on_startup` hook:** the sync migration/wait-check path bridges to
  Postgres through diesel-async's `AsyncConnectionWrapper` whenever the
  database URL requires TLS (`sslmode=require`/`verify-full` — the native
  libpq path can't reach a TLS-only server at all). That wrapper bridges
  every sync diesel call — connecting, and separately every query it's later
  asked to run — through its own internal `block_on`, which panics with
  "Cannot start a runtime from within a runtime" if invoked from a thread
  that is itself already inside some ambient tokio runtime's context. That's
  exactly what happens when an app or plugin calls a migration function
  (e.g. `autumn_web::migrate::run_pending(...)`) directly from a plain
  `async fn`/`.on_startup(|state| async move { ... })` body, rather than from
  `spawn_blocking`. Because the native (non-TLS) path never touches a
  runtime at all, this only ever surfaced once TLS was turned on — the exact
  "works in dev, breaks in prod against a TLS-only database" shape, and it
  could still resurface even after only the initial connect was fixed, the
  moment an actual migration query ran. The connect **and every subsequent
  query** now run together on one freshly spawned OS thread that never
  touches any ambient runtime, closing both failure points. Verified against
  a real TLS-enabled Postgres server, on both a `current_thread` and
  `multi_thread` tokio runtime, called directly from an async body.
- **Admin panel: a create/edit form that fails validation no longer discards
  everything the admin typed:** `POST /{slug}` and `POST /{slug}/{id}` — the
  only mutation UI the framework ships, used identically by every model in
  every admin-plugin deployment — propagated a malformed field (e.g.
  unparsable JSON) or a model-declared `AdminError::Validation` (e.g. a
  uniqueness check) as a bare `AutumnError`. That renders as a generic error
  response with no reference back to the form: a full-page navigation away,
  a "Go to homepage" link as the only recovery step, and every value the
  admin had entered gone. Both handlers now catch just that failure class and
  re-render the same form (HTTP 422) with every submitted value still filled
  in and the failure shown through the existing persistent, accessible flash
  banner — no toast, no blank form. On update, the redisplayed form is
  merged with the stored record so `create_only` fields (rendered read-only,
  never resubmitted) keep showing their real value instead of going blank.
  Other failure classes (missing pool, unknown model, database outage) are
  unchanged and still render the generic error page.
- **`distributed_lock`'s `cancelled_release_does_not_leak_lock` de-flaked and
  un-quarantined:** the test raced a `Duration::ZERO` `tokio::time::timeout`
  against a real `pg_advisory_unlock` round-trip, on the assumption that an
  already-elapsed timer always wins. It doesn't: `Timeout::poll` polls the
  wrapped future *before* checking its timer, so whenever the query happened
  to resolve within that same first poll the timeout never fired and the
  future was never actually cancelled mid-flight — measured at 1/30
  same-commit reruns. It had been `--skip`-listed out of `ci.yml`'s Docker
  sweep for this since, with no tracking issue. The test now polls
  `release()` by hand exactly once, asserts `Poll::Pending` (proof the query
  genuinely had not completed — a real network round-trip cannot resolve
  synchronously on its first poll) and drops it from there: no timing
  dependency, 0 failures across 50 reruns. Restored to CI's Docker sweep.
- **`autumn-cli export`'s `test_fetch_endpoint_success`/`test_fetch_endpoint_failure`
  de-flaked on `windows-latest` CI:** two independent mechanisms in the same
  hand-rolled mock-HTTP-server test harness (`export.rs`). The success-path
  server capped itself at `num_requests` *accept() attempts* rather than
  *served requests*, so a single transient `accept()` error (seen on
  windows-latest, where loopback connections are occasionally intercepted by
  Defender/firewall) silently gave up and left the client to time out with
  no response. The failure-path test "guaranteed" a closed port by binding
  an ephemeral port and immediately dropping it — but every test in the
  module runs concurrently and independently calls
  `TcpListener::bind("127.0.0.1:0")`, so the just-freed port could be
  reallocated to another test's mock server before this test's client
  connected, landing on a live, unrelated server instead of getting refused.
  The mock server now retries past a transient `accept()`/read failure
  instead of spending one of its `num_requests` slots on it, and the
  failure-path test now binds its own ephemeral listener and keeps it
  reserved (bound, unaccepted) for the request instead of dropping it —
  which both removes the race (nothing else can grab that port) and makes
  no assumption about what else might be listening on the host.

### Added

- **Admin user impersonation with an audit trail and a revert banner (#1394):**
  every support team eventually needs to "log in as this user" to reproduce a
  bug or verify a permission, and until now that meant hand-rolling a session
  swap — `session.set("user_id", target)` — which silently destroys the audit
  trail: from that moment on every version row and audit event claims the
  *customer* did it. A new additive API, `autumn_web::auth::impersonation`,
  makes the secure version the easy one. `begin_impersonation` swaps the
  session's effective user to the target and records the real admin separately
  under a reserved `impersonator_id` key, so the resolution the *framework* owns
  (`#[secured]`, `RequireAuth`, `PolicyContext`) transparently sees the
  **impersonated** user — `Auth<T>` is populated by the app's own loader
  middleware from request extensions, so it follows only if that loader reads the
  auth session key — while the framework's ambient current actor (#1383) — the
  value that seeds `#[repository(versioned)]` version rows and `AuditEvent`s — is
  published as the **real impersonator**. `end_impersonation` reverses it.
  Beginning is default-deny: it requires an `ImpersonationGate` in `AppState`
  (`ImpersonationGate::allow_roles(["admin"])` for the common case, or a custom
  `ImpersonationPolicy` — the seam where an app enforces its own tenancy
  boundary), so an app can never acquire impersonation by accident. Both
  directions rotate the session id (no fixation) and write an audit event
  carrying `{impersonator_id, target_id}`; a *denied* attempt is audited as a
  failure, and a begin is refused outright — rather than taking effect
  unrecorded — both when the audit write fails and when the app has installed
  no audit sink at all (`audit::write_from_state` is a silent no-op without
  one, so impersonation requires a real sink before it will swap anything). Impersonation does not nest — starting a second hop
  is a `409`, so it cannot be chained to escalate — and the impersonated
  session's role is resolved **server-side** by
  `ImpersonationPolicy::target_role`, never taken from request input, so an
  operator cannot mint a session more privileged than the target really is. The
  operator's step-up (`last_strong_auth_at`) claim is a bare timestamp with no
  identity bound to it, so begin stashes and drops it — otherwise a `#[step_up]`
  route could run a destructive action on the *target's* account on the strength
  of the operator's re-authentication — and end restores it. Impersonation also
  does not survive a change of identity: the record is bound both to the user it
  describes and to the session generation that created it, so either a different
  effective user or a session-id rotation (which every login performs) retires
  it — it stops counting for attribution and the revert route refuses it, rather
  than handing whoever comes next the operator's identity and role. The
  generation binding covers the case an id check alone misses: the impersonated
  customer signing in as themselves on the same browser.
  `impersonation::clear` scrubs it from a login flow outright. An `auth.session_key`
  configured as one of the reserved keys would make the swap clobber its own
  record, so both directions refuse it and registering the gate logs it at
  startup (`impersonation::is_reserved_session_key` /
  `RESERVED_SESSION_KEYS` expose the check).

- **`AdminPlugin::with_impersonation(gate)` — the impersonation UI (#1394):**
  opts an admin panel into the primitive above and mounts two routes:
  `POST {prefix}/impersonate` (behind the admin role gate, the step-up guard
  when enabled, *and* the `ImpersonationGate`) and `POST
  {prefix}/impersonate/stop`, deliberately mounted **outside** the role gate —
  while impersonating, the session carries the target's role, so a gated revert
  would trap the operator in the target's identity with no way back. The admin
  router also publishes the request's current actor unconditionally —
  attribution must not depend on the *optional* role check — so admin-surface
  writes are attributed at all (previously `SYSTEM_ACTOR`) and correctly while
  impersonating; runtime-config changes are recorded against the operator rather
  than the customer for the same reason. Every admin page then renders a
  persistent "Viewing as … — Stop impersonating" banner with one-click revert. The same banner goes in an application's own layout — the
  surface an operator actually looks at while impersonating a non-admin — via
  `autumn_admin_plugin::impersonation_banner_for(&state, &session, "/admin",
  csrf_token, csrf_form_field)` plus the exported
  `IMPERSONATION_BANNER_CSS`. Without `with_impersonation` neither route is
  mounted at all.
- **Examples for the four flagship 0.7.0 subsystems that shipped a guide and
  nothing runnable (#2320, T2):** the 0.7.0 docs audit found guide coverage at
  91.1% and example coverage at 59.4% — the debt had moved wholesale from docs
  to examples, and the release's four headline subsystems were the worst of it.
  Each now has a runnable example, and each example carries assertions rather
  than prose alone:
  - **Deterministic simulation testing** — `examples/reddit-clone/tests/sim_hot_rank.rs`
    is a seeded `#[sim_test]` over the app's `score / (age_hours + 2)^1.5`
    hot-rank decay curve. It walks 48 virtual hours in checkpoints through the
    ordinary `Clock` extractor, so it exercises the injected-clock seam rather
    than bypassing it, and finishes in milliseconds with no sleeping. `always!`
    carries the hard invariants (a rank never climbs as a post ages; a positive
    score never decays to zero) and `sometimes!` the reachability targets, with
    two deliberately-separated score bands so `assert_all_sometimes_satisfied()`
    holds at every seed and a green run is provably non-vacuous. It doubles as a
    regression test for the sim seams: a clock that stops reaching handler
    extractors makes every checkpoint equal and fires the monotonicity
    invariant.
  - **Failure capsules** — `examples/reddit-clone` gains an `autumn-capsules.toml`
    profile that arms `[failure_capture]` (kept out of the dev profile because
    turning capture on is a deliberate decision, not a convenience), a committed
    capsule recorded from `/dev/trigger-error` under `capsules/`, and a README
    walkthrough of `autumn replay` including the four verdicts and their exit
    codes. `tests/failure_capsule.rs` proves the profile really arms capture
    while `dev`/`redis` leave it off, that one failing request leaves exactly one
    capsule and a 404 leaves none, and that the committed fixture parses through
    the same `Capsule::from_json` the replay CLI uses. The profile also widens
    `[log] filter_parameters` to cover this app's `Stripe-Signature` intake
    header — redaction matches names by equality after normalization, never by
    prefix, so a prefixed secret header is recorded verbatim unless it is named.
  - **Self-clustering substrate** — `examples/bookmarks-distributed` already ran
    two web replicas behind nginx; they now form a two-node cluster with no
    coordination service, sharing a member view and a cluster-wide bookmark
    counter. `[cluster]` lives in `autumn-docker.toml` and the per-instance
    identity (node id, advertise address, seed peer) in `docker-compose.yml`,
    which also gains an explicitly addressed bridge network — `[cluster]` parses
    socket addresses and does not resolve hostnames, so the compose service
    names the rest of the stack uses are unusable as an advertise address. A new
    `/cluster` route reports the local view and the counter; unit tests assert
    the committed section passes the same `ClusterConfig::validate` that runs at
    boot, and that compose gives each replica a distinct dialable identity.
  - **App metrics facade** — `examples/bookmarks` records one domain counter
    (`bookmarks_created_total{outcome}`, counting rejected submissions as their
    own series rather than dropping them) and one timer
    (`bookmark_stats_query_seconds`, scoped to the two grouped aggregates behind
    `/bookmarks/stats` by resolving the guard with `stop()` before rendering),
    described once at startup so the timer's bucket bounds are set before
    registration freezes them. `src/metrics.rs`'s tests assert both instruments
    on `/actuator/prometheus` and under `/actuator/metrics`' `app` key with no
    database, and the Chromium smoke asserts the timer end-to-end against the
    real binary.

- **Capability-sandboxed plugins — install an unaudited plugin without
  installing its authority (#1609):** every Autumn plugin until now has been
  full-trust native code. `Plugin::build(self, app)` hands over the entire
  `AppBuilder`, which is the right trade for a first-party crate and the wrong
  one for something found on crates.io ten minutes ago — a compromised
  `autumn-plugin-*` can read your credentials, exfiltrate your database, or take
  the process down, and dependency auditing catches only the vulnerabilities
  someone has already named. The new non-default `plugin-sandbox` feature adds
  the other lane. A sandboxed plugin ships as a `.autumn-plugin` artifact — a
  `wasm32-wasip1` module plus a manifest declaring its route prefix, the exact
  `(method, path)` pairs it mounts, its capabilities, and its per-request CPU
  and memory ceilings — and the runtime enforces every word of it, refusing at load anything it cannot
  fully understand — including a route path `axum::Router::route` would panic on,
  which is validated through the same `matchit` engine axum routes through so a
  manifest can never take the application down at boot.
  Deny-by-default is structural rather than configured: the guest's whole
  authority is the host-function table the shim registers, so filesystem,
  network, environment and database access are not "off" but absent, each
  attempt answered with `ENOTCAPABLE`/`EBADF` and recorded as a logged denial,
  and an
  import no host function defines is refused at load before the artifact runs
  once. The manifest is the mount, not a description of it: the router is built
  from its declared routes, so an undeclared path under the prefix is a 404 the
  guest never sees. Fuel bounds CPU and a store limiter bounds memory, both
  per request against a fresh instance, and the interpreter runs on a blocking
  worker — so a spin, a memory bomb, a trap, a `proc_exit`, a malformed answer
  or no answer at all is a 502/503/504 on the plugin's own prefix while every
  other route keeps serving, and nothing a plugin does can abort the host
  process. Credentials are stripped from the request before it crosses, and
  `Set-Cookie`, framing headers and anything carrying `\r\n` are stripped or
  refused on the way back, so a plugin cannot forge a session in your origin or
  split your response. `autumn plugin package` binds a manifest to a module and
  stamps the digest the author could not know; `autumn plugin inspect` is the
  consent screen — the grant, the routes, the reviewed digest, every host
  function imported, the classes of authority denied — and it loads the module
  into the same sandbox the runtime uses and runs the existing route
  conformance checks over the manifest, offline. The app still deploys as one
  binary: `wasmi` is a pure-Rust interpreter, so there is no daemon, no
  subprocess and no native codegen backend. Purely additive — the native
  `Plugin` trait and every existing plugin are untouched, and the feature is not
  in `autumn-web`'s default set. The declared budgets bound the host's own
  work as well as the guest's: instantiating the module and encoding the request
  frame are both charged against `fuel` before they are performed, and anything
  the guest influenced — its error detail, its stderr, the interpreter's account
  of a trap — is truncated and control-escaped before it is logged, so a plugin
  cannot flood an operator's log or forge a record in it. First slice: request
  handling under the declared prefix is the only capability that exists, so no
  manifest can ask for a database, a session or an outbound call. See
  `docs/guide/sandboxed-plugins.md`.

- **UI/routing documentation and the flagship example that proves it (#2320):**
  the 0.7.0 docs audit found the UI/Routing block carrying the longest-standing
  doc gaps and almost no showcased example coverage. Four narrative guides and
  a reworked `examples/reddit-clone` close it.

  New guides:

  - `docs/guide/forms.md` — the changeset round-trip end to end: `ChangesetForm`
    and why a rejected submission is *data* rather than an error, `Valid<T>` /
    `Validated<T>` / `ValidateExt` for callers that are programs, model-level
    `#[validate(...)]` and the merged-model rule that makes it hold on updates
    **where it applies** — a repository with hooks or `validate_on_update =
    fetch` validates the merged model, while a plain generated repository takes
    the blind-update path and runs no model validation at all — plus
    `#[normalize(trim, downcase, upcase, squish, with = …)]` and exactly where
    it runs on the write path (before validation, before hooks, and on derived
    finders) **and where it does not**: both update paths persist the raw
    patch, so a direct `repo.update(...)` writes unnormalized values. CSRF,
    `form_for`, htmx inline validation and the no-JavaScript
    fallback that costs nothing to keep, accessible fields, and how to test the
    failure path. This was the last core-surface subsystem documented only
    obliquely.
  - `docs/guide/extractors.md` — the extractor catalog, the two ordering rules
    (one body extractor, and it goes last; head extractors run left to right,
    which is why `Db` must be dropped before a second pooled checkout), writing
    your own, and a full section on `Query<T>`'s structured decoding: repeated
    keys, `tags[]`, `tags[0]`, `filter[status]`, `items[0][sku]`, the depth cap,
    duplicate-key rejection, why errors never echo a value, and the
    `HashMap`-typed-target upgrade note.
  - `docs/guide/cookie-consent.md` — the gate as the actual compliance rather
    than the banner, the strictly-necessary exemption, `accept_all_cookie` /
    `reject_non_essential_cookie` / `expire_consent_cookie`, why accept and
    reject are `POST` while only the preferences page is a `GET`,
    `inject_consent_banner` and the four bugs its special cases prevent,
    policy-version re-prompting, and the GDPR Art. 7(3) withdraw flow.
  - `docs/guide/middleware.md` gains a "which hook do I reach for" decision
    table, a full `#[intercept(...)]` section (per-route tower layers, stacking
    order, when to use something else instead, and the `#[edge]` and idempotency
    trade-offs), and a section on the non-HTTP interceptors —
    `with_mail_interceptor` / `with_job_interceptor` / `with_db_interceptor` /
    `with_channels_interceptor` / `with_http_interceptor` — which no tower layer
    can reach.
  - `docs/guide/pagination.md` gains the `ListQuery` section the feature never
    had (allowlisted `sort` / `dir` / `filter[col]`, and why the allowlist lives
    in the generated `list()` rather than the extractor), plus a costs section:
    `COUNT(*)`, deep-offset scans, offset instability under concurrent inserts,
    and the two-connection deadlock.

  `examples/reddit-clone` now exercises the features those guides describe:

  - **Typed accessible forms** — the submit and edit forms are built from
    `a11y::TextField` / `TextArea` / `Select` / `Button` / `Link`, whose
    unlabeled forms do not implement `Render` and therefore do not compile.
    Errors are wired with `aria-invalid` plus `aria-describedby` pointing at a
    `role="alert"` message element.
  - **The changeset round-trip** — `submit` and `update` are `ChangesetForm`
    handlers that re-render the form on 422 with the author's title, URL and
    body intact, replacing hand-rolled `unprocessable_msg` errors that discarded
    the draft.
  - **Rich text** — post bodies are user-submitted Markdown rendered through
    `markdown::render_user_content` at display time, so the stored source stays
    editable and a later allowlist change protects posts already written.
  - **Cookie consent** — `inject_consent_banner` on spliceable HTML pages
    (registering the layer is not the same as "every page prompts": several
    response shapes are deliberately passed through untouched, all of them
    still receiving `Vary: Cookie` — `docs/guide/cookie-consent.md` enumerates
    them, and is the one place that list is maintained), `POST`
    accept/reject/withdraw routes, a `GET /consent/manage` preferences page
    reusing the framework banner widget, a footer link on every page, and the
    app's one non-essential category gated at its single call site.
  - **Pagination** — the community listing is offset-paginated with
    `PageRequest` + `Page` + `pagination_nav`, with page links that are plain
    `<a href>`, a self-referential canonical on deeper pages, and the live SSE
    feed wired on page 1 only.
  - **No-JavaScript fallbacks** — the forms carry no `hx-*` attributes and
    submit normally; unit tests assert that, the CSRF hidden input, the label
    and error wiring, and the rich-text sanitizer's script/image/scheme
    rejections.

- **Cache coherence is now proven at build time — the build fails when a write
  can leave a cached read stale (#1716):** Autumn’s cache was powerful and its
  coherence was entirely manual. `Cache::invalidate(key)` was hand-called,
  `#[cached]` memoized by argument hash, and nothing linked a cached value to
  the rows it was derived from or to the `#[repository]` write that dirties
  them. A forgotten invalidation shipped as a silent staleness bug — the most
  common and hardest-to-catch class of cache defect, and one no mainstream
  framework catches before production, because everywhere else cache keys are
  stringly typed and invalidation is convention. Autumn owns both ends of that
  dependency, so it can assemble the graph instead of hoping. `#[cached]` now
  publishes which models a value is derived from — declared with
  `reads(Post, Comment)`, or derived by the macro from the function’s own
  signature and body — and `#[repository]` publishes which model each of its
  write methods mutates. `autumn cache audit` reads both back out of the built
  binary (whole-app, so a read in one crate and a write in another or in a
  plugin are still compared), emits a stable-ordered, provenance-tagged
  cache-coherence manifest as a build artifact, and exits non-zero on any
  uncovered pair — naming the read, the write, the model they share, both
  source locations, and the two ways to discharge it. The obligation is
  discharged with `invalidates(path::to::cached_fn)` on the repository (or
  `#[invalidates(...)]` on one method), which is resolved by **rustc**: the path
  rewrites to the identity constant `#[cached]` emits beside the function, so an
  edge that names a non-cached function does not compile — it is a resolved
  path, not a string in a table somebody has to keep in sync. Alternatively
  `acknowledge_stale = "reason"` opts out, with a mandatory non-blank reason that
  lands in the manifest so every escape hatch is visible in review. A repository
  that declares any edge also gets a generated `invalidate_declared_caches()` that
  really does clear those reads — including on a shared cross-replica backend, via
  a new `Cache::invalidate_namespace` that `MokaCache` implements by iteration and
  `RedisCache` by a `SCAN MATCH` one segment narrower than its existing `clear`; a
  custom backend that cannot pattern-match its key space returns `false`, and the
  caller is told so rather than left believing the value is gone. A fill already
  in flight when an invalidation lands cannot write its stale value back
  afterwards: `#[cached]` samples the namespace’s epoch before the lookup and
  inserts through `with_fill_fence`, which re-checks it and inserts as one step
  that cannot interleave with the invalidation’s bump.
  Deliberately conservative in the direction
  that keeps the gate alive: a read whose dependency set could not be
  established is `undetermined` — reported in the manifest and the summary, never
  failed, unless `--strict` — because a checker that fails on what it merely
  could not read is a checker that gets deleted from CI. Reads the macros cannot
  see (fragment, read-through) declare themselves with `declare_cached_read!`,
  and `cache_fragment_in` / `cache_fragment_global_in` key a fragment under that
  declared id so the invalidation edge actually reaches it — the plain
  `cache_fragment` keys under a bare `fragment:` prefix that every fragment
  shares, which no per-read namespace sweep can match.
  `#[cached]` also gained `key(a, b)` to build the cache key from named
  parameters only, which is what lets a cached read take the repository handle it
  reads through — the handle is `Clone` but not `Hash`, and was never part of the
  value’s identity. A derived dependency resolves the model through the
  repository’s own `__AUTUMN_MODEL_NAME` rather than string-stripping its type
  name, so `#[repository(Comment)] trait ModerationRepository` yields `Comment`;
  a repository reached only through its trait is recorded `undetermined`, since
  the trait carries no such constant and a guessed model that names nothing
  would turn a real violation into a clean audit.
  What is proven and what is not is stated on the tin: the
  `invalidations` dimension carries a `runtime_caveat` saying the edge’s target
  is proven — rustc resolves it to the read’s own generated id constant — but
  its *invocation* on the write path is not, and the manifest’s `excluded` list
  names row/column granularity,
  cross-service coherence and TTL semantics as out of this slice. `examples/saas`
  demonstrates both halves — the app audits clean, and deleting the one
  `invalidates(...)` clause turns the build red. See
  `docs/guide/cache-coherence.md`.

- **Shadow (differential) deploys — mirror live traffic to a candidate build and
  diff its responses before cutover (#1653):** every deploy strategy Autumn
  shipped until now — rolling, blue/green, canary — routes **real** traffic to
  the new version and decides go/no-go from aggregate cohort metrics. That
  catches a build that falls over; it structurally cannot catch the build that
  returns `200 OK` with a dropped JSON field, a reordered list, or an off-by-one
  total, because nothing ever compares two responses to the *same* request. A
  new `[shadow]` section turns on in-process traffic mirroring: Autumn samples
  live `GET`/`HEAD` requests, replays each against an operator-provided
  candidate build, and diffs the two responses on status class and a normalized
  body. Object key order is normalized away; array order is not, because a
  reordered list is exactly the regression this exists to catch. Divergences are
  reported at `{actuator-prefix}/shadow` (sensitive-gated, like
  `/actuator/tasks`) and as the labelled metrics
  `autumn_shadow_comparisons_total{route,outcome}` and
  `autumn_shadow_divergences_total{route,kind}`; identical divergences collapse
  onto one record by a content-addressed `fingerprint`, so a captured pair is
  reproducible and one loud regression cannot evict every other record. The live
  request cannot tell mirroring is on: the shadow request is dispatched on a
  detached task and the primary response body is *teed* rather than buffered, so
  a slow, erroring, or unreachable candidate resolves to a counter and nothing
  else — bounded by `sample_rate`, `timeout_ms`, `max_in_flight` (excess is
  dropped, never queued), and `max_body_bytes` (an oversize body is skipped, not
  partially buffered). The candidate's response is read into a plain struct
  inside that task and dropped there, so no code path exists on which it could
  reach a user; only idempotent methods are mirrored, and the allowlist is a
  constant rather than a config key. Every mirrored request carries
  `X-Autumn-Shadow: 1`, which is both the recursion guard (a request carrying it
  is never mirrored again, so pointing a shadow at the app itself costs one
  extra request rather than an exponential storm) and the seam a candidate build
  uses to refuse writes. Recorded samples pass through the same
  `[log] filter_parameters` redaction as the access log and failure capsules,
  and an excerpt is recorded only when every scalar in the body has an object key
  above it, since the filter replaces a matched key's whole value and that is
  exactly when naming a key could reach it — an HTML body, a bare scalar (a
  `text/plain` one-time code parses as a JSON number) and a top-level array of
  strings all record a digest and a length instead — and the guide is explicit about what that redaction does and does
  not cover before you point this at a route returning personal data. Requests
  the live build itself refuses (`429`/`503` from maintenance mode, load
  shedding, or the rate limiter) are not mirrored at all, so a planned
  maintenance window does not read as a divergence storm. The candidate is
  dialed at `target` but sees the `Host` the live build accepted — behind a
  trusted proxy, the *resolved* authority rather than the internal raw header —
  so a candidate cloning production's `[security.trusted_hosts]`, or a
  subdomain-keyed multi-tenant app, does not diverge on every request; and in an SSG/ISG build
  the mirror sits outside the static-first middleware, so pre-rendered pages are
  compared rather than silently skipped. Off by default, and
  requires the `http-client` cargo feature (on by default) — a build without it
  says so at startup and mirrors nothing rather than pretending to. See
  `docs/guide/staged-deploys.md` for how it differs from canary and for the
  trust boundary (the candidate receives live credentials, and its own side
  effects are yours to contain). Effect virtualization for mutating traffic, and
  gating `autumn deploy` on a clean diff, are deliberate follow-ups.

- **Ledgered entities — time-travelable and tamper-evident by construction
  (#1699):** mark one entity `ledgered = true` and every insert, update and
  soft-delete appends an immutable, hash-chained revision carrying a **full row
  snapshot** and both time axes to the app's own Postgres or SQLite. You can then
  ask what a record looked like at any past instant (`ledger_as_of`), diff it
  across two instants (`ledger_diff`), and prove the stored history was never
  rewritten (`ledger_verify`) — without adopting a separate event store. The
  marker is the only per-model change: `ledgered` implies `versioned`, so every
  write path version history already covers appends a revision automatically,
  hand-written handlers, generated `api = "…"` endpoints, jobs, mailers, bulk
  saves, upserts and dependent cascades included.

  Reconstruction is byte-for-byte identical to what a plain query would have
  returned at that instant, pinned in CI against an oracle recorded live at each
  intermediate instant, on both storage tiers. Snapshots go through the model's
  durable per-field codec rather than serde, so `#[private]` and `#[encrypted]`
  columns — which a model omits from its public JSON — are preserved (encrypted
  ones as recoverable ciphertext, decrypted on the way back out), and the
  snapshot column is `TEXT` rather than `JSONB` so the stored bytes are exactly
  the bytes that were hashed.

  Revisions are bitemporal: `recorded_at` is transaction time, `valid_from` is
  valid time (defaulting to transaction time, or read from your own column via
  `ledgered(valid_time = "effective_at")`), so `LedgerAsOf::bitemporal` answers
  the auditor's question — what the database believed *then* about *then*.

  Each revision embeds its predecessor's hash, and `ledger_verify` reports the
  first broken link, distinguishing an edited row (`HashMismatch`) from one that
  was edited and re-hashed (`PrevHashMismatch` at the next link), a deleted
  revision (`MissingRevision`) and an inserted one (`DuplicateSeq`). Because a
  hash chain cannot prove that nothing is missing from its *end* — a truncated
  chain is internally perfect — `ledger_verify` also cross-checks the head
  against the live row, which additionally catches any write that reached the
  table without appending a revision (`LiveStateMismatch`). What remains
  undetectable is a *consistent* rewrite by someone with table access and the
  open-source hashing rule, so `ledger_head` exports the head hash for pinning
  outside the database; the migration's `(table, tenant, record, seq)` unique
  index makes a forked chain a write error rather than silent corruption.

  Because a ledgered entity's history *is* the record, every way of erasing or
  redacting it is refused at the repository seam at compile time: `ledgered`
  without `soft_delete` does not compile, `purge` (soft-delete's raw-`DELETE`
  escape hatch, which writes no history at all) is not generated,
  `#[version_history(sensitive = [...])]` is rejected because a redacted column
  could not be reconstructed, and a `dependent(..., on_delete = destroy)` cascade
  never erases a ledgered child — from a soft-deleting parent the child follows
  suit and records a revision, and from a hard-deleting parent the cascade is
  refused with a typed `LedgerError::HardDeleteCascade` naming the fix (the
  parent's macro cannot see that the child is ledgered, so this is the one guard
  that cannot be a compile error). `restore` — the inverse of a ledgered delete — records its own revision, so
  the ledger never silently disagrees with the table. `tenant_scoped` ledgered
  reads fail closed across tenants, and `across_tenants()` and cross-shard ledger
  reads are rejected rather than interleaving chains that share a record id. See
  `docs/guide/ledgered-entities.md`.

- **`autumn_web::data::csv::read_header`:** returns a CSV source's header column
  names in file order (empty for a source with no readable header). It exists so
  a caller can reject a file that is simply the WRONG FILE before importing any
  of it: `import_csv` decodes rows by column name and a decoder may legitimately
  default an absent field, so a spreadsheet sharing none of the expected names
  can parse cleanly into a run of blank records. The scaffolded CSV import below
  checks it against the columns the form can set.

- **`autumn_web::data::csv::count_data_rows`:** counts the data rows in a CSV
  source without retaining any of them — the header excluded, a malformed row
  included. It exists for callers that must bound how much work an untrusted
  upload can ask for *before* handing it to `import_csv`: a malformed row never
  reaches `import_csv`'s row handler (it is recorded as a `CsvRowError` and the
  parse moves on), so a counter inside that handler cannot see the very file
  that costs the most to accumulate. The scaffolded CSV import below uses it to
  enforce its row cap.

- **`autumn generate scaffold --import` — a CSV import route with a dry-run
  preview and per-row errors (#1393):** the symmetric counterpart to the
  scaffolded CSV export (#1315). One flag emits `GET /<plural>/import` (an
  upload form that prints the expected header row straight from the model's
  generated `CsvSchema`) and `POST /<plural>/import`, which parses the uploaded
  multipart CSV and — unless the submit explicitly confirms a commit — runs
  `import_csv` in `ImportMode::DryRun` and renders the `ImportReport`: rows
  read, rows that *would* insert, and a table of row errors with line numbers
  and messages. Ticking "Import for real" runs the same parse in write mode and
  commits through the repository's `save_many_skip_invalid`, so a row the
  database rejects is isolated and reported against its own CSV line instead of
  aborting the batch — inserted-versus-failed always adds up, with no silent
  drops. Each row is decoded, blank-normalized and validated through the module's
  own `decode_form` + `Changeset`, i.e. exactly the code path a browser form
  submission takes, and against the **same** `CsvSchema` impl the export writes
  from — one column map for both directions, so a file this app exported can be
  edited and uploaded straight back. The POST is `#[secured]`, renders the
  shipped CSRF and one-time submit-token inputs first (so both land inside the
  multipart token-scan window), caps the upload at an emitted
  `MAX_IMPORT_BYTES` on top of `security.upload.max_file_size_bytes`, and checks
  the file's extension and declared content type, and refuses a file whose
  header is missing any column the form can set — the check that catches an
  operator uploading the wrong spreadsheet, which row-level validation cannot
  see because each decoded row is valid. Work is bounded by rows as
  well as bytes (`MAX_IMPORT_ROWS`, mirroring the export's cap), rows past the
  cap is refused whole rather than imported as a prefix, and a write that fails
  partway through says which rows may already be committed instead of 500ing.
  The generated `tests/<name>.rs` gains a database-free test that uploads a
  2-row CSV (1 valid, 1 invalid) through the real `Multipart` extractor and
  `import_csv`, exercising the dry-run report (1 insertable row plus 1 row error
  on the right line, nothing written) and then the commit (exactly the valid row
  persists). Like every generated scaffold test it drives a stand-in resource
  rather than the app's own handler; the emitted handler itself is compiled and
  its import test run by the generator conformance suite.
  A file this app exported re-imports as the same VALUES: the import undoes the
  export's spreadsheet-formula apostrophe guard (on the text columns it applies
  to, and only those), normalizes the boolean spellings a spreadsheet writes,
  and an `--import` scaffold's `parse_local_datetime` also accepts the timestamp
  format the export writes. It re-imports as new RECORDS, though — the slice is
  insert-only, so re-uploading an exported file duplicates it; the upload page
  and the guide both say so. Columns the form does not carry (`id`,
  `created_at`, an `Attachment`, a `Bytea` — whose lossy export cannot
  round-trip — a `--default`ed column) are named on the page as
  ones the import cannot set, and a model with an at-rest `#[encrypted]` column —
  which the export omits but the form requires — or with no CSV-settable column
  at all (every column an `Attachment`, a `Bytea`, or `--default`ed, so an
  importer could only ever commit rows of defaults) — or a non-nullable `Bytea`
  column, which the import must skip but the form requires — refuses the surface
  outright
  with a warning naming the column.
  Additive: without the flag the scaffold's output is byte-identical, and
  `--import` on a variant that emits no `CsvSchema` (`--api`, `--live`,
  `--sharded`, an owner-scoped `--live-validation`) generates nothing and warns
  with the reason.
- **Web Push — notifications that reach a device with the tab closed
  (#1392):** Autumn already generated an installable PWA (#1149) and stored an
  in-app notification feed (#1148), but a notification could only ever arrive
  while the user already had the app open. The new `autumn_web::push` module
  closes that loop, and a developer writes **zero** lines of crypto: the
  framework mints/loads the VAPID key pair, signs the ES256 identity JWT
  (RFC 8292), performs the ECDH + HKDF + AES-128-GCM payload encryption
  (RFC 8291), and dispatches to the push service. Configure `[push]
  private_key`, mount `autumn_web::push::router()` (which `autumn generate pwa`
  now does for you), and call `push.send(user_id, &PushMessage::new(title,
  body).url(target))`.

  `autumn generate pwa` now also emits a service worker with `push` and
  `notificationclick` handlers, a client subscribe snippet wired to the
  framework's own public-key endpoint, and a `push_subscriptions` migration —
  all idempotent, `--dry-run`-honoring, and fully reversed by
  `autumn destroy pwa`.

  Subscription storage is hardened against the fact that an endpoint URL is a
  *capability*: the subscribe boundary normalizes the endpoint (so one browser
  can never become several rows), caps subscriptions per principal, and allows
  an endpoint to move between accounts only when the request presents the
  stored `p256dh` — which keeps the shared-device case working (the browser
  returns the same endpoint **and** keys) while refusing a takeover by anyone
  who merely learned the URL. The outbound transport resolves the endpoint host
  and refuses any address on the framework's SSRF deny-list, pins the connection
  to the checked address, and declines redirects, so neither a DNS record
  pointing at `169.254.169.254` nor a `307` can steer the POST at an internal
  host.

  `[push]` honors the usual environment overrides
  (`AUTUMN_PUSH__PRIVATE_KEY` and friends) — with one deliberate divergence: a
  *blank* private-key override is preserved and refused at boot rather than
  clearing the setting the way other `AUTUMN_*` overrides do, because the
  commonest cause is a secret that failed to interpolate, and clearing it would
  silently disable delivery (and erase a good key from `autumn.toml`). The
  `subject` is validated at boot
  against what RFC 8292 permits — a bare email address instead of a `mailto:`
  URI otherwise boots cleanly and has every delivery refused remotely. Because
  Autumn's CSRF layer rejects an unaccompanied POST and its cookie is
  `HttpOnly`, the public-key response carries the caller's CSRF token for the
  generated snippet, which is what keeps push opt-in working under the
  production defaults without exempting the push routes from CSRF.

  Because every endpoint is client-chosen, the delivery path is written for a
  hostile one: a principal's devices are dispatched to concurrently (bounded),
  so a device that accepts a connection and never answers cannot hold up its
  live siblings; the transport reads only the status code and discards the
  response body unread, so a registered endpoint cannot stream unbounded memory
  into the process; and the generated snippet unsubscribes on sign-out, so a
  subscription never outlives the session that created it.

  Failure posture is deliberate: a `[push] private_key` that is present but
  unusable fails the **boot** rather than leaving push silently dead, and
  sending with no key configured is an error raised before any dispatch, never
  an `Ok` report of zero deliveries. A `404`/`410` from the push service prunes
  the subscription so a dead endpoint is never re-sent to, while a `5xx` or
  rate limit leaves it in place — pruning on a transient failure would silently
  unsubscribe every user during an outage. Because the subscribe endpoint takes
  a client-supplied URL the framework later POSTs to, it requires `https` and
  refuses IP-literal and loopback hosts, closing the SSRF shape at the
  boundary.

  The encryption is pinned to RFC 8291 §5's own published test vector — fed the
  RFC's inputs it must reproduce the RFC's output byte-for-byte — and the
  end-to-end test decrypts a dispatched body back with the receiving user
  agent's private key and verifies the VAPID signature, so what is asserted is
  what a real browser would receive. Ships `RecordingPushTransport` so
  applications can assert their own push behaviour the same way.

  Adds **no new crate** to the dependency graph: `p256` was already resolved
  for `jsonwebtoken`'s ES256 backend, and `aes-gcm`/`hmac`/`sha2`/`base64` were
  already non-optional dependencies. All additions are additive — a new
  `[push]` config block, a new `push_subscriptions` table, and new public API;
  no existing `autumn-web` surface changed. Guide:
  `docs/guide/web-push.md`. Out of scope by design: native mobile push
  (APNs/FCM device tokens), notification actions/images/badges, and per-user
  preferences or quiet hours.
- **SEO guide and a runnable SEO example (#2320, T1 Gap 1):** `seo.rs` was
  rustdoc-only and `docs/guide/` had zero `sitemap`/`robots` mentions, even
  though the `seo(...)` route attribute shipped in 0.7.0. The new
  [`docs/guide/seo.md`](docs/guide/seo.md) covers the `seo(...)` argument and
  its keys, the `SeoMeta` extractor and its Open Graph/Twitter fallbacks,
  canonical URLs, `robots = "noindex"` and exactly which sitemap entries it
  filters, `[seo]`/`[seo.robots]` configuration, `SitemapSource` (including
  the start-up-snapshot semantics and the custom-route escape hatch for larger
  or live sitemaps), the 50,000-URL limit, `hreflang` alternates on
  locale-prefixed sites, and static builds. `examples/reddit-clone` is now the
  runnable sample: a database-backed `RedditSitemapSource` (entry-capped, with the cap's cost characteristics documented) whose `<lastmod>` is derived from the whole page — the latest of `posts.updated_at`, the newest live comment, and the newest comment deletion — rather than read from one column, `[seo]` +
  `[seo.robots]` in `autumn.toml`, `seo(...)` on the front page, the community
  index, the community page, the post page, and the about page, canonical URLs
  on each of them, `robots = "noindex, nofollow"` on the submit form, and a
  single `SeoMeta::render()` call in the shared layout. Covered by unit tests
  in `routes/layout.rs`/`seo.rs` and a Postgres integration suite in
  `tests/seo_pg_integration.rs`.

- **`#[query_budget(N)]` — a compile-time, per-route database query budget
  (#1667):** declare a handler's query ceiling and the build fails when any
  statically reachable path can exceed it. The canonical case is the N+1 — a
  repository or `Db` call inside a loop over a runtime-sized collection — and
  the diagnostic names the offending call site and the loop it sits in. Because
  the gate runs during `cargo build` it fires on every branch, tested or not,
  where the existing runtime tools (the dev inspector's N+1 badge, and
  `TestResponse::assert_max_queries`) only catch a regression when its exact
  path happens to be exercised. Autumn can attribute queries to a route
  statically because it owns 100% of the query-issuing surface: the handle is
  always named in the handler's signature, so straight-line statements sum,
  `if`/`match` arms take the worst arm, a handle-rooted chain is one query
  however many builder methods it carries, and
  `.preload(rows, Post::preload().author().tags())` is one batched query per
  association. The analysis is conservative by construction: a loop whose body
  queries is unbounded unless the iterable has a literal bound, a repository
  future is counted where it is *built* (so collecting futures for `join_all`
  is still an N+1), and anything unreadable — a helper handed the handle, a
  macro body naming it, a closure that may run per element — is reported rather
  than assumed query-free. Three escape hatches keep legitimately dynamic code
  compiling: `#[query_budget(unbounded, reason = "…")]` on the handler, and
  `#[query_cost(N)]` / `#[query_exempt(reason = "…")]` on a statement. Each
  annotated function also emits a hidden `StaticQueryBudget` constant recording
  what was declared and what was proved. The analysis is written against
  autumn's real query surface: `Db::tx` / `tx_with` callbacks are counted once
  and their `conn` is tracked, `#[model]` static finders (`Post::published(&mut
  db)`) count as one query, repository builder chains cost the same whether or
  not they are split across `let` bindings, and `find_in_batches` / `find_each`
  are unbounded because a keyset walk issues one query per batch. Handles are
  followed through fields and conventional accessors (`self.repo`, `state.db`,
  `app.pool()`) so a service method's queries are visible too. Adopted in
  `examples/bookmarks`. See
  [the query budgets guide](docs/guide/query-budgets.md).

- **`AppBuilder::plugin_migrations(name, migrations)`:** a named registration
  path for embedded Diesel migrations owned by a plugin or other third-party
  integration, distinct from the app's own `.migrations()`. Diesel's
  `__diesel_schema_migrations` table is keyed by version alone, so it is
  entirely normal for two independently authored migrations — the framework's,
  a plugin's, the app's own — to reuse the same version by coincidence (e.g.
  `examples/todo-app`'s own first migration collides with the framework's
  legacy `create_api_tokens` migration, both using the all-zero placeholder
  version). Applied naively, whichever set runs first would "win" the
  version and the other's same-versioned migration would be skipped forever
  as "already applied", even though its `up.sql` never actually ran. Rather
  than reject this — which would leave an app unable to use a plugin at all
  until someone renames a migration in a dependency they may not control —
  the framework now detects the collision at apply time (across every
  registered source, including ones the framework itself folds in after
  app-wiring time) and transparently tracks one of the colliding migrations
  under a bounded, deterministic substitute version, so **both** migrations
  still apply. Which one keeps the plain version is a pure function of the
  migrations' own names (not registration order), so reordering
  `.migrations()`/`.plugin_migrations()` calls or adding a new plugin never
  flips an already-settled collision. This is logged at `INFO` so it is
  visible, not silent, and it applies uniformly to Postgres and SQLite
  targets. A version reused under the exact same full migration name (e.g. a
  shard-required set folded verbatim into another bundle too) is the
  separate, intentional, harmless case and is left untouched. This does not
  (and, given Diesel's version-only tracking, cannot) recover history for a
  new plugin whose migration collides with a version an app already applied
  under an *earlier*, pre-plugin deploy — see the method's doc comment.
  A migration's generated substitute is salted with its OWN full name (fixed
  by its directory naming), never with a source name or the changing set of
  sources that register it — so the substitute stays stable across releases
  even as a duplicate bundle is later folded into an additional plugin, and
  is independent of which registration happens to run first. A generated
  substitute is also checked against every raw version already claimed by
  any registered migration, not just other substitutes, so it can never
  coincide with an unrelated migration's own version. `autumn migrate
  status`/`autumn migrate down` resolve applied versions against the app's
  own `migrations/` directory only, with no visibility into which plugins
  were registered at runtime — if the app's own migration is the one that
  loses a collision, those two CLI commands cannot currently resolve or
  revert it by name, and the migration checksum/drift-detection system
  likewise can't see it (so a later edit to that migration would not trigger
  the drift guard); see the method's doc comment for the manual fallback and
  the (accepted, very-low-probability) edge case around a future migration's
  raw version coinciding with an already-applied substitute. Collision
  resolution also now includes the two standalone shard-directory / shard-map
  control migrations, which are applied straight from their own `const`s
  rather than through the app's registered set — a plugin claiming one of
  their (fixed, framework-owned) versions under a different name previously
  skipped past this guard entirely — but only when the app has shards
  configured at all, so an unsharded or `sqlite` app (which never applies
  either set) never has its own migrations' versions perturbed by a
  collision against one it will never actually record.

### Security

- **consent-banner middleware now guards the already-rendered case against
  shared-cache CSRF token replay:** when a handler (e.g. a "manage cookie
  preferences" page) had already rendered the consent banner itself —
  detected via `RENDERED_BANNER_MARKER` — `splice_into_response` returned
  that response unchanged without stamping `Cache-Control: private,
  no-store` / `Vary: Cookie`, even though the handler's own rendering
  embeds a live, per-visitor CSRF token (`consent_banner_markup`). A shared
  cache sitting in front of an otherwise-cacheable handler could therefore
  serve one visitor's token-bearing form to another, leaking the token and
  causing that visitor's subsequent submission to fail CSRF validation. The
  same two headers the injection path already sets are now applied on this
  path too.

- **`"all"` is now a reserved search index name:** `AUTUMN_SEARCH_BACKFILL`
  and `autumn search reindex --index` use the literal value `all`
  (case-insensitive) as a sentinel meaning "every registered index." Nothing
  previously stopped an app from registering a real index actually named
  `all` (a valid identifier, so `IndexDefinition::validate` accepted it) —
  requesting a rebuild of that specific index instead reindexed, or with
  `--purge`, emptied, every index in the app. `IndexDefinition::validate`
  now rejects `all` (case-insensitive) at index-registration time, so the
  ambiguity can never reach a backfill.

- **the edge-capsule runtime now bounds a dispatched handler's response body
  at 16 MiB:** `EdgeHandler` accepts any axum handler return type, including
  a streaming body whose length is unbounded or driven by request input.
  `serve_io` collected a dispatched handler's response with no limit before
  emitting the wire frame, so one such request could buffer without bound —
  the wasm guest's own allocator eventually traps against the host's memory
  budget, but only after the attempt, and `serve_io` can also run natively
  with no wasm host in the loop at all, where nothing else bounds it.
  Exceeding the new cap now fails closed into the same
  `FallthroughReason::CapsuleError` fallthrough the runtime already uses for
  every other decline, so the origin serves the request instead.

### Performance

- **new `throttle_check` profiling harness; findings, no fix:** added
  `autumn/benches/throttle_check.rs`, driving real traffic through a
  `#[throttle]`-guarded route and an identical unthrottled route at
  equal-length paths with an identical response body (issue #1350's
  per-route rate limiter had no committed benchmark before this). Several
  review rounds fixed real measurement bugs before these numbers were final:
  `key = "token"` with an identical `Authorization: Bearer` header sent to
  BOTH routes (not `key = "ip"` — `TestApp` requests carry no `ConnectInfo`,
  which would make `extract_throttle_key` return `None` and profile
  `__check_throttle`'s no-client bypass instead of the real
  `limiter.decide()` path; and not an asymmetric header, which would fold
  its own construction cost into the measurement); equal-length routes/body
  (`/route-a`/`/route-b`, both `"ok"`), since differently-sized paths and
  response bodies also leaked into the byte delta; a `THROTTLE_LIMIT` guard
  plus an assertion on every measured response, so a large `--iterations`
  can never silently drain the bucket and profile denials instead of the
  documented warm `Decision::Allowed` path; asserting (not just
  `black_box`ing) the measured status on BOTH routes, since an earlier
  version asserted only the throttled side, putting that assertion's own
  comparison/branch instructions asymmetrically into the callgrind delta;
  and widening the frame-level DHAT attribution beyond `rate_limit::`-named
  frames to also catch `#[throttle]`'s generated `FromRequestParts` gate
  cloning `parts.headers` *before* ever calling into the `rate_limit` module
  (`autumn-macros/src/throttle.rs`), which the first attribution pass missed
  entirely; and base-subtracting the callgrind instruction counts (an
  `--iterations 0` run per route, matching the DHAT methodology) rather than
  dividing raw process totals by request count, which had been diluting both
  routes' per-request figures with shared process-startup/router-construction
  cost. Full, corrected `#[throttle]` overhead against the
  ~140-151-block/~27.3-28.7KB per-request baseline `config_alloc_gate`
  already gates (#2232): ~10 blocks / ~885 bytes per request (~6.6%/~3.1%,
  under the 10%-of-allocations floor) and ~5.3% more instructions than an
  unthrottled route on the marginal, base-subtracted count (callgrind,
  `--route throttled` vs. `--route plain`) — which, read as "would a fix
  removing this entire overhead clear the 5%-of-instructions floor,"
  technically says yes, though only just. But no *safe, autonomous,
  smallest-fix* candidate gets there: the only narrowly-scoped,
  mechanistically-clear piece —
  two redundant `format!` calls building an almost-always-cache-hit
  `HashMap` key (`resolve_throttle_params`'s `registry_key`,
  `__check_throttle`'s `cache_key`) — accounts for only ~6 of those ~10
  blocks and isn't separately visible above a 1%-of-instructions self-cost
  threshold on its own. The rest (two `HeaderMap` clones, one in the
  generated gate and one in `extract_throttle_key`, plus the LRU-backed
  `MemoryStore::decide` bucket lookup) is load-bearing rate-limiting state
  tracking, not an obvious redundancy, and this bench's minimal 1-2-header
  requests likely *understate* the gate's `parts.headers.clone()` cost for
  a real multi-header production request. Fixing the aggregate would mean
  restructuring the limiter's per-request key derivation and bucket-lookup
  path together — a maintainer decision on a security-relevant surface, not
  an unreviewed autonomous change. Recorded as a findings issue rather than
  shipped; the harness itself is the lasting artifact, giving `#[throttle]`
  its first profiling coverage.
- **new `repository_crud` profiling harness; findings, no fix (#2486):** added
  `autumn/benches/repository_crud.rs`, driving real `save`/`find_by_id`/`page`
  calls through a `#[repository]`-generated repository against a live
  Postgres — the query-building/row-mapping layer `request_pipeline.rs`
  deliberately excludes (its handlers are trivial and non-DB by design) and
  that no other committed bench touches. Profiling it turned up that every
  repository call currently pays for two liveness-style round trips beyond
  its actual query — diesel-async's connection pool defaults to a `SELECT 1`
  ping on every checkout, and `#[repository]`'s generated
  `__autumn_acquire_from` re-runs `SET statement_timeout` on every checkout
  too, even though the two overlap: `SET statement_timeout` already fails on
  a dead connection the same way the ping would. An isolated A/B (the pool's
  recycling method only, no framework code changed) measured instructions
  -13.00%, allocation blocks/round -14.44%, allocation bytes/round -16.20%
  (`valgrind --tool=callgrind`/`dhat`). No source fix here: switching the
  framework's own pool-builder default changes connection-liveness-detection
  behavior for every deployed app, which is a maintainer call rather than an
  unreviewed automated change — see #2485 for the full mechanism and
  reproduction steps.
- **`ledger_as_of`/`ledger_diff` no longer read a ledgered record's entire
  revision chain to answer a question with one answer:** both are pure
  functions over `ledger_revisions(record_id)`, which has always had one SQL
  shape — read every stored revision, every full-column snapshot, in `seq`
  order — with no bound on the requested instant at all. For a hot ledgered
  record (an account, invoice, or contract adjusted repeatedly over a long
  operational life, exactly the shape the feature exists for), an audit query
  about *recent* history — the overwhelmingly common case — paid for reading
  the record's whole history regardless. Both now issue a bounded
  `ORDER BY seq DESC LIMIT 1` lookup instead, using the same index
  `ledger_revisions` already relied on — no migration, no new index. Because
  transaction time is monotonic in `seq` (#2323), a transaction-time as-of
  query's cost is now proportional to how far back the question asks, not to
  the chain's total length. `ledger_diff`'s two endpoints are resolved from
  one `UNION ALL` statement on one connection rather than two independent
  lookups, so it keeps the single-snapshot consistency the old full-chain
  read had (two separate connection-acquiring calls could otherwise resolve
  `from`/`to` against two different database states if a write landed, or a
  read replica advanced, between them) — statement count for `ledger_diff`
  is unchanged at 1. Measured (`pg_stat_statements`, testcontainer Postgres,
  three ledgered chains at 300/700/1,200 revisions each, written through the
  real write path): a near-head as-of query's buffers fall 86–94% and stay
  flat across all three depths (22/129/132 → 3/8/8), instead of scaling with
  chain length; `ledger_diff` across a recent window falls 129 → 16 buffers
  (-88%). Disclosed trade-off: the pathological case — asking about a
  record's very *first* revision on a deep chain — reads more buffers than
  before (132 → 948 on the 1,200-revision fixture), because the backward
  index scan can't short-circuit when the answer sits at the far end of it;
  this is bounded by the record's own chain depth and only reachable by a
  query about a record's oldest history, so it does not offset the
  unconditional win on the realistic (recent-history) query pattern.
  `ledger_revisions` and `ledger_verify` are unchanged — they legitimately
  need the whole chain and still read it.

- **the DB-backed media-room reaper sweeps in one statement instead of one per
  stale room:** `DbRoomStore::reap_stale`'s second phase — the sweep that drops
  now-empty rooms, run every 60s by the background room reaper in any process
  wiring in `room_store_backend = "db"` — loaded every stale-room candidate,
  then issued a `SELECT COUNT(*)` per candidate and a `DELETE` per now-empty
  one. That is O(n) statements per tick, where n is however many rooms went
  stale since the last tick, so a busy multi-tenant deployment paid it in
  proportion to its own traffic. The emptiness test is now a correlated
  `NOT EXISTS` on the participants' composite key inside the delete itself, so
  the whole phase is a single anti-join statement. Measured against an 8,704-row
  production-shaped fixture with 8,002 stale candidates (`pg_stat_statements`,
  testcontainer Postgres): phase-2 statements per tick 15,504 → **1**, phase-2
  buffers 78,512 → 41,109 (**-47.6%**). No schema change, no new index. The
  sweep keeps its exact reap set, its last-write-wins idempotence, and its
  namespace isolation — and, being one atomic statement, no longer has a
  per-candidate window between the occupancy check and the delete.

- **a no-database app no longer compiles the framework's database codegen:**
  `autumn-macros` had no `[features]` section at all, so `model.rs` and
  `repository.rs` — together ~40k of the crate's ~60k lines, and the bulk of its
  compile time — were built in full for every consuming app, including one that
  can reach neither macro (`autumn-web` already gates both re-exports on its own
  `db` feature). `autumn-macros` now has a default-on `db` feature covering that
  codegen, and `autumn-web` takes the crate with `default-features = false` and
  forwards its own `db` to it, so a DB-free app skips the modules instead of
  compiling them and throwing them away. Measured on a 4-core box, a debug build
  of `autumn-macros` drops from 31.1s to 2.7s (-91%) with `db` off; the crate sits
  on the serial critical path of a first build (nothing else starts until it
  finishes), so that time comes straight off cold-start onboarding. End to end,
  `autumn dev-loop-bench --cold-start` — `autumn new` to the first HTTP 200 for
  the no-DB starter — drops from 136.6s to 90.8s (-33%) on the same box. That
  does not on its own bring the gate's p95 60s / max 90s budget green, so the
  budget question issue #2309 raises stays open (issue #2309). A DB-backed app
  enables `db` and is unaffected — same macros, same expansions. The serde /
  JSON-schema field helpers shared with
  `#[derive(OpenApiSchema)]` moved out of `model.rs` into a new always-compiled
  `schema` module, so the derive keeps working (and keeps its tests) with `db`
  off. Direct dependants of `autumn-macros` see no change: `db` is on by default
  there.

- **`autumn-search`'s Postgres backend now batches a document write into one
  statement instead of one per document:** `PostgresSearchStore::write_documents`
  — the single write path behind both `SearchBackend::index` and
  `SearchBackend::index_unless_newer` — looped over its `documents` slice and
  issued one `INSERT ... ON CONFLICT DO UPDATE` per document. `SearchClient::backfill`
  drives this with up to 500 documents per call, so a full-index backfill cost
  one DB round trip per row indexed. All documents in one call now land in a
  single multi-row statement (a plain `VALUES` list when unconditional, a
  `UNION ALL` of guarded `SELECT`s when watermark-guarded), cutting statement
  count from one-per-document to one-per-backfill-batch. Every per-field SQL
  expression is unchanged; only how many round trips carry it changed. See
  `docs/reports/2026-08-25-ledger-search-write-documents-batch/`.

- **the admin panel's bulk "delete" action now issues one `UPDATE` instead
  of one per selected row:** `AdminModel::execute_action`'s trait default
  (what `POST /admin/{slug}/actions` calls for every model that doesn't
  override it) looped over the submitted ids and called `delete()` once
  per id — a full connection checkout plus a single-row `UPDATE` per id, so
  an operator selecting hundreds of rows in the admin list and clicking
  "Delete selected" cost hundreds of round trips. `TokenAdminModel` (the
  built-in `/admin/api-tokens/` model) now overrides `execute_action` for
  `"delete"` to batch every id into one
  `UPDATE api_tokens SET revoked_at = ... WHERE id = ANY($1) AND revoked_at IS NULL`
  round trip; the returned count and end state are unchanged (a duplicate,
  already-revoked, or nonexistent id is still a silent no-op, still counted
  as "applied"). Measured against a 50,000-row fixture with a 2,050-id bulk
  action: revoke statement calls 2,050 → 1. See
  `docs/reports/2026-08-31-ledger-admin-bulk-delete-batch/`.

- **`FeatureFlagAdminModel`'s admin panel bulk "delete" action now issues
  one `DELETE` CTE instead of one per selected flag:** it never overrode
  `AdminModel::execute_action`'s trait default, so it inherited the same
  per-id loop the `TokenAdminModel` fix above closed — a full connection
  checkout plus a single-row `DELETE ... WHERE id = $1 RETURNING key`
  (feeding the `feature_flag_changes` audit insert) per id. It now
  overrides `execute_action` for `"delete"` to batch every id into one
  `WHERE id = ANY($1)` round trip; the returned count, final row state,
  and audit trail are unchanged (an already-deleted or nonexistent id is
  still a silent no-op, still counted as "applied"). Measured against a
  4,000-row fixture with an 820-id bulk action: delete-CTE statement
  calls 820 → 1, buffers 9,639 → 6,977 (-27.6%). See
  `docs/reports/2026-09-06-ledger-feature-flag-admin-bulk-delete-batch/`.

- **scaffolded form helpers no longer re-escape their own constant HTML at
  render time:** `text_input`, `password_input`, `textarea_input`,
  `number_input`, `checkbox_input`, `date_input`/`datetime_input` (and their
  `required_*`/`*_htmx` variants) build their `class`/`aria-invalid`
  attributes from an `if has_errors { A } else { B }` expression where `A`
  and `B` are always one of a small, fixed set of compile-time string
  literals (`"autumn-field__input"`, `"true"`/`"false"`, etc.). Passed to
  `maud::html!`'s `(...)` splice as plain `&str`, each of those literals ran
  through `maud::escape_to_string`'s byte-by-byte HTML-escaping scan on
  every render even though the value can never contain a character that
  needs escaping. Wrapping the literals in `maud::PreEscaped(...)` — the
  same mechanism the form helpers already use elsewhere for known-safe
  content — skips the scan entirely; behavior and output are unchanged.

  Measured with the committed `autumn/benches/form_render.rs` harness (a
  realistic 12-field scaffolded form, two fields carrying validation errors,
  3,000 iterations = 3,050 renders), `valgrind --tool=callgrind`, before and
  after on the same machine:

  | | before | after | delta |
  | --- | ---: | ---: | ---: |
  | Instructions (3,000-iteration run) | 220,034,852 | 203,355,183 | **-7.58%** |
  | `maud::escape_to_string` instructions | 65,998,950 (30.0%) | 48,339,450 (23.8%) | **-26.8%** |

  Allocation counts are unchanged (`escape_to_string` writes into an
  already-allocated buffer; the win is pure CPU, not allocations) — all 203
  `form`/`nested_form` lib tests pass unchanged.

- **scaffolded form helpers no longer allocate a validation-error id string
  on fields that have no error:** `text_input`, `password_input`,
  `textarea_input`, `number_input`, `checkbox_input`,
  `date_input`/`datetime_input` (and their `required_*`/`*_htmx` variants),
  plus the newer `a11y`-routed `wrap_field_control`, unconditionally built
  `format!("{field}-error")` up front, even though the string is read only
  when that field actually carries a validation error. On a realistic
  scaffolded form most fields don't — the committed 12-field
  `autumn/benches/form_render.rs` workload has errors on 2 of 12. The fix
  defers the `format!` behind `has_errors.then(...)`, allocating only when
  the id is actually needed; output is byte-for-byte unchanged (verified by
  the unchanged 203 form/nested_form lib tests).

  Measured with a new allocation gate
  (`autumn/tests/form_render_alloc_gate.rs`, `allocation-counter`, same
  12-field workload) and `valgrind --tool=callgrind` on the existing
  `form_render` bench (3,000 iterations = 3,050 renders), before and after
  on the same machine:

  | | before | after | delta |
  | --- | ---: | ---: | ---: |
  | Instructions (3,000-iteration run) | 203,331,276 | 185,748,259 | **-8.65%** |
  | `alloc::fmt::format::format_inner` instructions | 6,734,400 (3.31%) | 3,928,400 (2.11%) | **-41.7%** |
  | Allocation blocks (200 renders) | 24,800 | 20,800 | **-16.1%** |
  | Allocation bytes (200 renders) | 4,498,200 | 4,495,800 | -0.05% |

  Bytes barely move — each wasted allocation is a handful of bytes against a
  ~22KB/render budget dominated by larger buffers — but the block-count and
  instruction-count deltas both clear the impact floor on their own.

- **scaffolded form helpers pre-escape their own field values, labels, and
  error text instead of letting `maud::html!` re-scan them byte by byte:**
  `text_input`, `password_input`, `textarea_input`, `number_input`,
  `checkbox_input`, `date_input` built every dynamic string — the field's
  current value, its label, its `id`/`name`, and any validation-error text —
  through `maud::html!`'s ordinary `(expr)` splice, which calls
  `maud::escape::escape_to_string`: a loop that matches and pushes one byte
  at a time even when nothing needs escaping. Profiling the committed
  `autumn/benches/form_render.rs` workload showed that loop was 26% of the
  release-build instruction count on a realistic 12-field form.

  A new `Esc` type implements `maud::Render` directly (`autumn_web::form`)
  and writes the escaped bytes straight into `html!`'s own output buffer
  via one bulk `push_str` per clean run instead of one call per byte — an
  earlier version of this fix pre-built an owned, escaped `String` and
  handed it to `maud::PreEscaped`, which a reviewer (Codex) correctly
  flagged as a second, avoidable copy for any value that actually needs
  escaping; writing straight into `html!`'s buffer needs only the one copy
  `escape_to_string` was already doing, in every case. `fast_escape` (used
  only to build `id`/`name`-derived strings like `"{field}-error"` ahead of
  a `html!` block, where the result has to be a concatenatable `str` rather
  than something written into a buffer) still returns the input completely
  unallocated when nothing needs escaping. Output is byte-for-byte
  unchanged — checked against a naive reference by a proptest over
  arbitrary strings (including multi-byte UTF-8), and by the unchanged 209
  `form`/`nested_form` lib tests.

  Measured with the committed `autumn/benches/form_render.rs` harness and
  the existing `autumn/tests/form_render_alloc_gate.rs` allocation gate
  (both the same 12-field workload), `valgrind --tool=callgrind`, before
  and after on the same machine:

  | | before | after | delta |
  | --- | ---: | ---: | ---: |
  | Instructions (3,000-iteration run) | 185,711,127 | 159,908,641 | **-13.90%** |
  | `maud::escape::escape_to_string` instructions | 48,339,450 (26.0%) | 0 (eliminated) | **-100%** |
  | Allocation blocks (200 renders) | 20,800 | 20,800 | unchanged |
  | Allocation bytes (200 renders) | 4,495,800 | 4,604,600 | +2.4% |

  Allocation *count* is unchanged — escaping never allocates for this
  fixture's clean values (and, with the direct-`Render` design, never
  allocates a temporary buffer even when a value *does* need escaping).
  Bytes rose slightly: `maud_macros` sizes each `html!` block's output
  buffer from the *source token length* of the macro invocation, not
  runtime content, and the interpolations calling into `Esc`/`fast_escape`
  read as "expect more output" than the plain, unescaped splices they
  replaced. That reservation is never grown again (the unchanged block
  count proves it), so it's unused slack, not extra allocator work — see
  `form_render_alloc_gate.rs` for the full explanation and its updated
  ceiling.

- **`[compression]`-enabled apps no longer pay an extra ingress box level for
  it (#2371):** every other config-gated member of `apply_middleware`'s
  single merged `Router::layer` tuple composes in via a plain
  `tower::util::option_layer`, but `CompressionLayer` changes the response
  body type, which `option_layer`'s `Either` cannot absorb (both of its
  branches must share one `Response` type) — so `apply_compression_middleware`
  kept its own standalone `Router::layer` call, the one member of the ingress
  stack #2198 didn't fold in. That call boxes the whole downstream stack in a
  fresh `BoxCloneSyncService` that every request above it deep-clones, the
  same quadratic-per-layer cost #2193/#2198 fixed for the rest of the stack.
  Compression now folds into the merged tuple too, paired with
  `NormalizeBodyLayer` — the same body-type-adapter the tuple already uses
  elsewhere — so `option_layer`'s two branches agree on a `Response` type
  again. Compression is off by default, so this is a pure win for the apps
  that turn it on and a no-op otherwise, confirmed by
  `middleware_stack_depth.rs`'s `compression_enabled_does_not_deepen_the_framework_cascade`
  (traversal count no longer moves when `[compression] enabled = true` is
  toggled) alongside the unmoved default-feature `INGRESS_TRAVERSAL_WINDOW`
  gate. `AssetCacheControlLayer`, the event-bus/oauth tuple, and the
  outermost `SecurityHeadersLayer` remain separate `Router::layer` calls —
  each has a genuine ordering/scope constraint (route-mount timing,
  dev-profile-only middleware injected between it and the rest of the stack,
  and MCP-dispatch-clone timing respectively) that folding would change the
  behavior of, not just its cost.
- **scaffolded form helpers build their `-field`/`-error` id suffix by direct
  string concatenation instead of `format!`:** `text_input`, `password_input`,
  `textarea_input`, `number_input`, `checkbox_input`, `date_input` each built
  `let wrapper_id = format!("{field_html}-field");` and (when the field has an
  error) `format!("{field_html}-error")` — a fixed two-part concatenation of
  already-`&str` pieces, but routed through `format!`'s `Arguments`/
  `fmt::Write` dispatch and an unsized `String::new()` that grows via
  reallocation as the pieces land, rather than a single up-front
  `String::with_capacity`. Profiling the committed
  `autumn/benches/form_render.rs` workload showed `alloc::fmt::format::format_inner`
  and `core::fmt::write` alone at 2.46%/2.00% of the release-build
  instruction count, plus a further ~4% in the `String`-growth machinery
  (`RawVecInner::finish_grow`/`reserve::do_reserve_and_handle`) those two
  calls' unsized starting buffer drove. A new private `concat_suffix(base,
  suffix)` helper (`String::with_capacity(base.len() + suffix.len())` plus
  two `push_str` calls) replaces all 12 call sites (2 per helper × 6
  helpers); output is byte-for-byte unchanged (verified by the unchanged 209
  `form`/`nested_form` lib tests).

  Measured with the committed `autumn/benches/form_render.rs` harness and the
  existing `autumn/tests/form_render_alloc_gate.rs` allocation gate (both the
  same 12-field workload), `valgrind --tool=callgrind`, before and after on
  the same machine:

  | | before | after | delta |
  | --- | ---: | ---: | ---: |
  | Instructions (3,000-iteration run) | 159,827,570 | 138,424,533 | **-13.39%** |
  | `alloc::fmt::format::format_inner` instructions | 3,928,400 (2.46%) | 0 (eliminated) | **-100%** |
  | `core::fmt::write` instructions | 3,202,591 (2.00%) | 0 (eliminated) | **-100%** |
  | Allocation blocks (200 renders) | 20,800 | 18,000 | **-13.5%** |
  | Allocation bytes (200 renders) | 4,604,600 | 4,565,600 | -0.85% |

  Clears the impact floor two ways independently: >=5% instruction reduction
  on the realistic, directly-exercised bench workload, and >=10% allocation-
  block reduction per the gate. The block-count drop is larger than the
  12-call-site count alone would suggest — `format!`'s unsized starting
  buffer apparently cost more than one allocation for some of these calls
  once it had to grow, not just the final `String`.

- **the versioned-repository audit write path stops cloning every column
  (#2429):** `compute_diff` / `compute_insert_changes` /
  `compute_delete_changes` take `&serde_json::Value`, so each one had to clone
  every retained column name and value into the `ColumnChange` it returns. The
  only production caller — the `#[repository(versioned = true)]` codegen, which
  every insert, update and delete of a versioned model funnels through — builds
  those `Value`s fresh from `version_column_values()` per mutation and drops
  them the moment the diff returns. Every retained field was therefore allocated
  twice and dropped twice, for a value nobody would look at again.

  Three owned-value siblings — `compute_diff_owned`,
  `compute_insert_changes_owned`, `compute_delete_changes_owned` — take the
  `Value` by value and move each key and value straight into its
  `ColumnChange` (`Map::into_iter` / `Map::remove` instead of `.get().cloned()`
  / `.clone()`). The codegen now calls those. The borrowed functions are
  unchanged and stay public, for callers whose `Value` outlives the call.

  Allocation blocks per mutation on a realistic 7-column record, measured by the
  new `version_history_alloc_gate` (`allocation-counter`, deterministic across
  runs). Each figure includes the throwaway `Value` materialization both paths
  are charged for, so the delta is only what each does with the value it is
  handed:

  | Entry point | Borrowed | Owned | Delta |
  |---|---|---|---|
  | `compute_insert_changes` | 26 blocks | 14 blocks | **-46.2%** |
  | `compute_delete_changes` | 26 blocks | 14 blocks | **-46.2%** |
  | `compute_diff` | 34 blocks | 27 blocks | **-20.6%** |

  Whole-process totals over the committed `autumn/benches/version_history.rs`
  harness, one variant per process (`BIN borrowed` / `BIN owned`):

  | Metric | Borrowed | Owned | Delta |
  |---|---|---|---|
  | Instructions (`valgrind --tool=callgrind`) | 2,275,350,133 | 1,547,245,235 | **-32.0%** |
  | Allocation blocks (`valgrind --tool=dhat`) | 6,901,045 | 4,441,045 | **-35.6%** |
  | Allocation bytes (same) | 422,257,918 | 393,179,915 | -6.9% |

  Wall clock on the same harness moves with it — insert and delete land around
  -33% (roughly 650 -> 420 ns/op), and the harness now reports median and
  spread across alternating rounds so a figure inside the noise says so. The
  `compute_diff` row is the small one either way: it only ever retained the
  *changed* columns (3 of 7 in this workload), so it had the fewest clones to
  drop.

  This is an audit surface, so the changeset itself is held byte-identical
  rather than eyeballed: each owned function is asserted equal to its borrowed
  twin — as a `Vec<ColumnChange>` *and* as the serialized JSON that reaches the
  `_autumn_version_history` row — over shared case matrices (25 diff cases, 10
  insert/delete cases) covering sensitive-column redaction, columns present in
  `after` but not `before`, columns dropped from the changeset, `null`-vs-
  missing, non-object inputs, and key ordering. Separate tests assert pointer
  identity between the input buffers and the emitted ones, so an `_owned`
  function that quietly delegated to its borrowed twin would fail even though
  its output is correct.

## [0.7.0] - 2026-08-23

For a narrative tour of this release, see the
[0.7.0 release walkthrough](docs/releases/0.7.0.md); for the upgrade path from
0.6.x, see the [migration guide](docs/migrations/0.7.0.md).

### Added

- **`autumn deploy` prepares the host and migrates on the first deploy (#1607):**
  the two remaining gaps between `autumn deploy` and its acceptance criteria are
  closed, so the documented target-host precondition is now genuinely "a stock
  Ubuntu LTS with SSH access" and the first deploy of a database-backed app needs
  no out-of-band migrate step.
  - **Host preparation.** `autumn deploy up` no longer requires you to install
    `kamal-proxy` yourself. It probes the target read-only for a working proxy and,
    on a host that has none, installs the pinned build at
    `/usr/local/bin/kamal-proxy` (plus `curl`, which the readiness gate uses **on
    the host**, and a container runtime — kamal-proxy publishes no release binaries,
    so the pinned `basecamp/kamal-proxy` image is the source). Only genuinely
    missing packages are installed, the image is pinned **by digest** (a Docker Hub
    tag is mutable, and the binary is executed as root), the binary is staged and
    moved into place so the supervised path is never half-written, the install
    refuses outright if anything already exists at that path, and it verifies the
    binary it landed before the deploy continues. Host preparation is
    Debian/Ubuntu-only and needs outbound HTTPS; a failure names exactly that, plus
    the opt-out. A host that already has a working proxy is **untouched**;
    a proxy that responds but whose CLI surface has **drifted** is still never
    replaced — that stays the actionable, fail-closed refusal it has been. In a
    fleet the install is the first op of that host's own turn, not part of the
    all-hosts probe phase, so "no host is touched until every host is graded" still
    holds and a failed install halts and compensates like any other pre-cutover
    failure. Decline it with `[deploy] install_proxy = false` (default `true`, env
    override `AUTUMN_DEPLOY__INSTALL_PROXY`) if you provision the proxy yourself.
  - **Migrations on the first deploy.** `first_deploy_ops` now runs the same
    blocking `AUTUMN_MIGRATE=1` one-shot the redeploy path does — **before** the
    initial release is started, so the app never boots against a schema that was
    never applied, and a failed migration aborts the deploy with nothing started and
    nothing routed. Consequently the fleet's single migration moves to the **first
    host in rollout order** whatever its mode: an all-first-deploy fleet migrates on
    host 1 instead of migrating nowhere, the schema always moves before any host cuts
    over, and both of the migration-ordering hazard warnings a scale-up used to
    print are gone because neither state is reachable any more. The one-shot now
    also loads the release's own uploaded `autumn.toml` (`AUTUMN_MANIFEST_DIR`,
    matching the slot unit) and runs in the release dir (`--working-directory`,
    matching the unit's `WorkingDirectory`), so neither a config-only database
    topology nor a relative database URL is resolved differently by the migration
    than by the app it gates; and the fleet summary stops claiming "the migration that
    already ran was NOT rolled back" for a rollout that failed *before* reaching its
    migration.
  - The container end-to-end test asserts the first deploy's migration over real
    ssh (a marker written by the one-shot itself), and the nightly real-VPS
    workflow now provisions a **stock** image, asserts it has no kamal-proxy, and
    proves `autumn deploy up` installs a working one — its manual bootstrap step is
    gone, which is what makes the "≤ 3 commands from `autumn new`" metric honest.

- **Translatable model fields — `#[translatable]` (#1384):** declare a `#[model]`
  column translatable and it stores an **independent value per locale tag**,
  resolving on read against the request's active locale with **no locale
  argument in the handler**. The field's type becomes
  `autumn_web::i18n::Translated`, a per-locale container persisted as a JSON
  object in the column's own `TEXT` storage (portable across Postgres and
  SQLite); `Display` renders the active locale, so `html! { h1 { (post.title) } }`
  serves Spanish under `Accept-Language: es` and — untranslated under `fr` —
  falls back through **`I18nConfig::resolved_fallback_chain`, the same chain UI
  strings walk**. Resolution is `Bundle`'s own algorithm (exact active locale,
  then each chain link in order); once the chain is exhausted the field returns
  a single documented sentinel — `None` from `resolve()`, the empty string from
  `Display` — never a panic or a 500. The active locale is published by an
  `AmbientLocaleLayer` that Autumn installs alongside the translation bundle
  (and inside each `/{locale}/…` nest, so locale-prefixed routing composes);
  reads outside a request — a job worker, a scheduled task, a test — fall back
  to the process-wide chain rather than panicking, and `i18n::with_locale(tag,
  fut)` scopes one explicitly. Because the value **is** the whole map, an
  ordinary `find` → `set_title("es", …)` → `update` round trip changes one
  locale and leaves every other byte-for-byte intact, and `Serialize` stays
  lossless so record version history and durable commit-hook payloads can't
  destroy the other locales on replay. `Deserialize` is its exact inverse — a
  map, and only a map: a **bare string is refused**, because assignment replaces
  the whole container, so `PUT /api/posts/1 {"title": "Hola"}` would otherwise
  delete every other language from a body that reads like it sets one field.
  `Translated::merge_from` is the non-destructive path for a partial update.
  Resolution survives a **streaming** response (the layer wraps the response
  body, so an SSE or chunked render still resolves per frame), and a deeper
  `Locale` extraction refines the ambient locale in place, so the two can never
  disagree on paths where the layer sits outside the session. The macro emits
  `post.title_localized()` / `title_in(locale)` / `set_title(locale, value)` /
  `title_locales()` / `title_is_translated(locale)` per field, plus the
  field-name-keyed `available_locales("title")` / `is_translated("title", "fr")`
  / `Post::translatable_fields()` an app renders a "needs translation"
  affordance from, and registers each column for surfaces with no compile-time
  view of the model. The field DSL gains a `{translatable}` modifier
  (`autumn generate model Post 'title:String{translatable}'`) which emits the
  attribute, the `Text` `schema.rs` entry, and a migration that is a plain
  `ADD COLUMN … TEXT NOT NULL DEFAULT '{}'` — `autumn migrate check` classifies
  it as **safe**, with no new operation type and no backfill job. A stored value
  that is not a JSON object of strings reads as the default locale's
  translation, so an existing plain-text column can be declared `#[translatable]`
  with **no data migration** (an object is read as translations when every value
  is a string; keys are not inspected, so every key an app can write is
  guaranteed to round-trip). Purely
  additive and opt-in per field: a model without the attribute is unchanged
  apart from an empty `__AUTUMN_TRANSLATABLE_COLUMNS` constant, and the UI `t!`
  path is untouched. `autumn generate model` enables autumn-web's non-default
  `i18n` feature when any field is translatable, so the emitted project builds.
  `generate scaffold` deliberately **refuses** a `{translatable}` column (its
  single-input CRUD form would replace the whole container on save) and names
  `generate model` instead; the macro and the DSL likewise refuse combinations
  whose halves disagree about the column's contents — `#[encrypted]`,
  `#[searchable]`, `unique`/`indexed`, `#[normalize]`, `#[state_machine]`,
  `#[id]`, `#[lock_version]`, `#[position]`, the `{min}`/`{max}`/`{email}`/
  `{url}` validation fan-out, `#[serde(rename)]` and `#[diesel(column_name)]` —
  each naming the reason and the alternative, and the `--unique`/`--index`/
  `--searchable`/`--shard-key` flag spellings are refused alongside the `{…}`
  ones. See [i18n](docs/guide/i18n.md#translatable-model-fields).
- **Multi-server fleet deploys — `[deploy] hosts` (#1621):** list several
  SSH-reachable servers under `[deploy] hosts` (mutually exclusive with the
  single-server `[deploy] host`; also `AUTUMN_DEPLOY__HOSTS` as CSV) and the same
  `autumn deploy up` becomes a **rolling deploy across the fleet**. Either env
  spelling, set non-empty, now **clears the other spelling from `autumn.toml`**,
  so the documented env-over-TOML precedence retargets a single-host project as a
  fleet (or a fleet project at one server) instead of tripping the
  mutual-exclusion refusal; setting *both* env spellings non-empty is still
  refused as an ambiguous rollout order, and an empty or blank value still means
  *unset* and leaves the TOML spelling alone. The list
  order *is* the rollout order: hosts are replaced strictly one at a time, each
  running the unchanged per-host blue/green cutover against its own kamal-proxy
  and finishing before the next is started, so the rest of the fleet keeps
  serving throughout. One release id per run, and **migrations run exactly
  once** — on the first host in rollout order, before its cutover; every other
  host skips them. (Superseded within this release: the placement was originally
  the first host still on a *previous release*, which left an all-first-deploy
  fleet migrating nowhere and made scale-up host order load-bearing. Now that a
  first deploy migrates too — see the `autumn deploy` entry below — the migration
  simply lands on host 1, so the schema always moves before any host in the fleet
  cuts over and neither hazard warning exists any more.) A failure
  **halts** the rollout (the
  remaining hosts are never touched) and, by default, compensates the hosts that
  already cut over in reverse rollout order — rolling each back to its previous
  release, or removing a just-completed first deploy — best-effort-continue and
  never silently: post-cutover *housekeeping* failures (`record-proxy-options`,
  `drain-old`, `prune`) leave the host live and healthy, so the rollout warns and
  keeps going, while a host whose rollback target is in doubt (markers left
  mid-transaction, a missing or unverifiable target dir, no recorded previous
  release) is reported with the exact by-hand recovery command instead of being
  guessed at. Compensation and `autumn deploy rollback` restore **binaries only**
  — a migration that already ran is never rolled back, which is stated on every
  surface that can reach that state. The closing `Fleet state:` summary states it
  in whichever of three tenses is TRUE, gated on the **migration** having been
  reached rather than on whether the fleet compensated: a host still on the new
  release gets the forward-looking `the schema has moved …` note; a fleet that
  actually *restored* a host or *removed* a just-completed first deploy gets `the
  compensating rollback restored BINARIES only …` (a compensation that merely
  **failed** no longer claims this — that host is still serving the new release,
  so it takes the forward-looking note instead, and both notes appear together
  when a halt compensated some hosts and left others forward); and the case that
  previously printed **nothing at all** — no host forward and nothing to
  compensate, because the migrating host itself failed after `migrate` but before
  its cutover and tore its own candidate down — now gets its own `no host is
  serving the new release, but the migration that already ran was NOT rolled back
  …` note. Every non-empty rollout schedules a migration, so the gate on all three
  is simply whether host 1 was reached at all.
  (A failed *single-host* deploy still renders no summary and so still says
  nothing about a migration that already ran — tracked as #2276.)
  `--only <HOST>` (repeatable) narrows `up` or
  `rollback` to a subset as a repair lever, warning loudly every time that the
  fleet may end up mixed; `--no-rollback` halts and freezes for inspection
  instead. `autumn deploy rollback` rolls the whole fleet back newest-first and
  exits non-zero unless every host came back. Fleet-unsafe topologies fail closed
  before any remote command runs — a `sqlite://` database (N independent files),
  `[media.mediamtx]` (no teardown path), and `[deploy.tls]` (terminate TLS at the
  load balancer that fronts the fleet) — and `[database] auto_migrate` on a fleet
  is a loud warning, as are the per-process background-work defaults
  (`[jobs] backend = "local"`, `[scheduler] backend = "in_process"`), which are
  correct on one host and become N independent queues and N cron timers on a
  fleet. The preflight grades **every** targeted host before any host is touched,
  and `autumn doctor` grades the same list; it is also the fail-fast boundary that
  `up`, `check` and `rollback` all abort on, so it now **fails closed on a fleet
  with no entries at all** (#2274) — returning the same `no target host
  configured` `ssh_reachability` failure the single-host path uses, rather than
  zero checks, which would read as "0 failed" and walk the run past the gate into
  a panic downstream. Note that no host is ever *drained*
  for the rollout's sake — a host's `/ready` never goes `503` and it is never
  removed from your load balancer's pool; each host is replaced in place by its
  own kamal-proxy flipping loopback slots. See `docs/guide/fleet-deploys.md`.

- **`autumn deploy status` — read-only fleet state and drift (#1621):** one row
  per configured host in rollout order — mode, deployed release (from the
  `current` symlink), live slot, `/ready` status, maintenance flag and proxy port
  — plus **version drift** (hosts on different releases) and **state drift**
  (per-host marker damage that will fail that host's *next* deploy closed: a
  `live-slot` marker disagreeing with the running proxy, an unreadable
  `shared/proxy-options`, a proxy bound to a different public port than
  `[server] port`, no release deployed while the rest of the fleet serves one,
  and a `current` symlink that resolves to no readable release). It mutates
  nothing, so it is safe mid-incident; an unreachable host is a row, not an
  abort, and a host whose release cannot be read is reported as unknown and
  explicitly **not** counted as *version* drift — though a reachable host that
  proved it has a `current` symlink and still resolves to nothing readable is
  state drift, so `--strict` exits non-zero on it. The **maintenance column
  reports the flag file that host's *running* slot unit polls**, resolved on the
  host from that unit's `Environment=AUTUMN_MAINTENANCE_FLAG_FILE` (falling back
  to its `WorkingDirectory` plus the legacy relative
  `tmp/autumn-maintenance.json`) rather than reading the shared path
  unconditionally — which would report `off` for a maintained host whose unit
  polls elsewhere and `ON` for a host still taking traffic. It is therefore
  three-valued: `maintenance ON` / `maintenance off` / `maintenance ?`, the last
  when the live slot unit could not be read at all, which is never rendered as a
  confident `off`. Two matching state-drift reasons come with it — an unreadable
  live slot unit, and a host whose app polls a release-local maintenance flag
  instead of the shared one (a unit predating that override; the remedy is to
  redeploy it), so a host deployed before this feature reports its release-local
  flag until it is redeployed. The column states which file the running unit
  polls and whether that file exists — not the app's in-memory state, which
  follows on its own 500 ms poll. `--strict` exits non-zero on any drift so it is
  alertable from cron; `--json` emits a stable report, in which `maintenance` is
  correspondingly `true` / `false` / `null` (`null` = which file that host's unit
  polls could not be proved) — both `false` and `null` stay falsy, so an existing
  `maintenance == true` check is unaffected. Unlike `deploy check`, `up` and
  `rollback`, `status` does **not** abort when the application config fails to
  validate under the deploy profile: it needs only `[server] port`, so it prints
  a caveat on stderr (in `--json` mode too, leaving stdout's shape untouched)
  naming the config error and the declared port it probes against, and reports
  the fleet anyway. The state-changing commands still refuse — they grade and
  upload runtime values, so an invalid config must stop them. See
  `docs/guide/fleet-deploys.md`.

- **`autumn deploy maintenance on|off` — fleet-wide maintenance over SSH
  (#1621):** turns [maintenance mode](docs/guide/maintenance-mode.md) on or off
  on every deploy-configured host, with the same flags as the local
  `autumn maintenance on` (`--message`, `--allow-ips`, `--readonly`,
  `--bypass-header`) and the same wire format, so running apps react within their
  500 ms poll with no restart and no deploy — unlike the local command, which
  only writes the machine it runs on. The authoritative shared flag
  (`{app_dir}/shared/autumn-maintenance.json`) is written first; for a host whose
  slot unit predates that override, the file *that unit* polls is written too,
  resolved from the host's **live slot unit** — the slot the proxy is serving —
  and never from the `current` symlink, which is rewritten after the proxy flip
  and so can name a release nothing is running. Best-effort-and-aggregate: every
  host is attempted, the per-host table names what changed, and the hosts that
  *did* change are deliberately not reversed (that would reopen the window being
  closed) — the "Changed anyway" line lists only the fully-changed ones. The
  command exits non-zero if any host failed, and it **fails closed on a partial
  change**: a host whose shared flag was written but whose running unit's own
  file was not (its unit could not be read, or that write failed) renders a
  `PARTIAL` row and counts as a failure, so `on` never claims a host is
  maintained when it could not prove which file that host polls, and `off` never
  claims to have removed one. A host with no promoted release is the one
  shared-flag-only case, and it is a success. Like `deploy status`, the fan-out
  keeps running when the app config fails to validate under the deploy profile —
  same stderr caveat, then the declared `[server] port` read without validation,
  used only to identify which slot unit each host runs. Every surface repeats the rule
  that matters: **maintenance does not drain a host from a load balancer** —
  `/ready` stays `200` by design, so a maintained host keeps taking traffic and
  answers it with `503`. See `docs/guide/maintenance-mode.md`.

- **`autumn deploy` hardening that also applies to a single host (#1621):** a
  one-entry `hosts` list behaves exactly like `host`, but a single-host deploy is
  *not* unchanged from the previous release — these apply to every deploy,
  whatever the host count. The deploy now refuses a **release-directory
  collision** — the release id has one-second granularity, so a fast re-run could
  re-upload into the directory `shared/previous-release` still points at and make
  a later rollback roll *forward* — and refuses equally when the probe cannot
  prove the directory is absent; concretely, a re-run of `autumn deploy up`
  inside the same second now exits non-zero where it previously proceeded, at the
  cost of one extra read-only round-trip before anything is mutated. Every
  `ssh`/`scp` invocation also carries `ConnectTimeout=10`,
  `ServerAliveInterval=15` and `ServerAliveCountMax=4`, so a host that accepts
  TCP and then wedges produces a finite error instead of hanging the deploy
  forever. Slot units now carry
  `Environment=AUTUMN_MAINTENANCE_FLAG_FILE={app_dir}/shared/autumn-maintenance.json`,
  so an active maintenance flag survives a cutover instead of being orphaned by
  it — which also means the **local** `autumn maintenance on`, run on a
  deploy-managed host, writes a path the app no longer reads: use
  `autumn deploy maintenance` there. Three smaller changes complete the list: the
  ops that complete a cutover append an advisory fragment recording a
  `shared/last-deploy` marker naming the action that completed (what
  `deploy status` reads, and it can never fail the op it rides on — a
  compensated first-deploy teardown records `torn down`, so a host with nothing
  installed never reads back as deployed); the existing
  `detect-current` probe resolves the `current`
  symlink in the same round-trip via one extra delimited section; and the "no
  target host configured" hint now names `[deploy] hosts` alongside
  `[deploy] host`. See `docs/guide/deployment.md`.

- **`Query<T>` decodes sequences and nested structures (#1972):** the extractor
  no longer delegates to `serde_urlencoded`, whose strict flatness meant a
  `Vec<String>` field fed the conforming `?tags=a&tags=b` form failed with
  `invalid type: string "a", expected a sequence`, and a nested struct field was
  unrepresentable by any encoding — so builders fell back to comma-separated
  strings and JSON-in-a-string. It now decodes a **superset** of the flat form
  (a query string of unique scalar keys behaves exactly as before) that also
  accepts repeated keys (`tags=a&tags=b`), the append and indexed sequence forms
  (`tags[]=a`, `tags[0]=a`), nested objects (`filter[status]=open`), and arrays
  of objects (`items[0][sku]=A-1`) — the same bracketed dialect
  `NestedChangesetForm` already uses for `has_many` rows. Scalar coercion,
  present-but-empty `Option` handling, and unknown-key tolerance keep
  `serde_urlencoded` parity; decode errors now name the failing field path
  (`filter.limit: invalid value …`). Nesting is depth-capped and indices key an
  ordered map rather than a `Vec`, so neither deep nesting nor `tags[4000000000]`
  can drive unbounded allocation.
- **MCP `tools/call` dispatch honors structured query arguments (#1972):** the
  dispatcher already expanded an array query argument to repeated keys, which
  the handler then rejected — a tool whose derived `inputSchema` advertised
  `tags: array` dispatched a request its own handler answered with 400. Query
  arguments are now rendered into the extractor's own wire format (scalars flat,
  scalar arrays as repeated keys, containers bracketed), so the advertised
  contract and the dispatch actually agree. A JSON `null` renders no parameter
  at all instead of the literal text `null`, and nesting past the decoder's cap
  is refused up front. An argument the encoding cannot carry — an empty array or
  object, a `null` array element, an object field name containing `[` or `]` —
  is an invalid-params error rather than a silently altered dispatch, and one
  call's query expansion is bounded. The build-time "nested query field" warning
  is gone — nested query fields are now honored rather than steered away — while
  the opaque-`{"type":"object"}`-placeholder warnings remain.

- `#[commentable]`'s generic router gained `CommentsConfig::on_comment`, an
  after-commit hook carrying the created comment (ids, author, body) so an app
  adopting the router keeps its own side effects — notifications, live-feed
  broadcasts, search indexing. `examples/reddit-clone` uses it to keep
  announcing new comments on `/ws/feed`.
- **Threaded, polymorphic comments — `#[commentable]` (#1367):** Autumn's fifth
  association kind. `belongs_to`/`has_many`/`has_one`/`through` all pin the
  child to exactly one parent table, which is why every app that wants comments
  on a *second* model duplicates the table, the routes, the threading query and
  the count maintenance. `#[commentable(by = User)]` on a `#[model]` replaces
  all of it: one `comments` table keyed on `(commentable_type,
  commentable_id)`, with a `parent_id` self-reference for threading, attaches to
  any number of models at once. The attribute emits `Model::COMMENTABLE_TYPE`,
  `Model::commentable_spec()`, a `{Model}Comments` trait
  (`add_comment(parent_id, author_id, body, reply_to)` /
  `comment_thread(parent_id)` / `delete_comment(comment_id)`) blanket-implemented
  over the generated repository, and an `inventory` registration — so
  `AppBuilder::nest("/comments", autumn_web::commentable::router(…))` serves
  **every** commentable model in the binary from one pair of routes, and adding
  a third model needs no route, no query and no new table — just its own
  `comment_count` column. `comment_thread` is
  one query whatever the nesting depth (the tree is assembled in Rust, never an
  N+1 walk) in stable `(created_at, id)` order; `delete_comment` cascades to the
  whole descendant subtree and is idempotent. `parent.comment_count` is
  maintained by the #1325 counter-cache primitive — a single atomic `SET c = c +
  $1` **inside the comment's own transaction** — and the thread read is
  soft-delete aware. Because a single column cannot reference two tables, the
  write path *is* the referential check: `add_comment` probes and row-locks the
  parent before inserting, so an unknown, soft-deleted or foreign-tenant parent
  is a `404` before anything is written, and a `reply_to` naming a comment on a
  different record (or one deeper than `max_depth`, default 5) is a `422`. The
  view half is `widgets::comment_thread`, a no-JavaScript nested `<ol>` with an
  inline `<details>` reply form on every node; with htmx present each form
  additionally swaps the thread region in place. `autumn generate scaffold post
  title:string comments:commentable` emits the shared table (once per project),
  the `comment_count` column, and the attribute. See
  `docs/guide/commentable.md`.

- **`autumn generate webhook` for signed, replay-safe provider intake
  (#1366):** the `SignedWebhook` substrate has shipped since 0.4.0, but every
  Stripe/GitHub/Slack integration still hand-rolled the route, the endpoint
  config, the event dispatch, and the signature tests — security-sensitive
  boilerplate (raw-body ordering, constant-time compare, replay window, secret
  rotation) nobody should retype. One command now emits it: a
  `#[post("/webhooks/<provider>")]` handler taking the shipped extractor (no
  hand-rolled HMAC), an `event_type()` dispatch skeleton with marked stub
  functions and an acknowledge-and-ignore default arm, the route registered in
  `routes![…]`, and an `autumn.toml` `[[security.webhooks.endpoints]]` stub that
  references the signing secret by `secret_env` (never inline) with replay
  protection on. Provider presets `stripe`, `github`, `slack`, and `generic`
  map onto `WebhookProvider`, including the Slack Events API `event_callback`
  envelope and its `url_verification` challenge handshake. The endpoint block is
  all the wiring needed — Autumn installs the registry from it and derives the
  path's CSRF/submit-token/CAPTCHA exemptions from it on every boot, so no
  stale-prone copies are written — and `[security.webhooks.replay]` is emitted
  explicitly with Redis guidance, since production validation rejects the
  process-local `memory` backend for replay-protected endpoints. The
  generated `#[cfg(test)]` module signs a fixture delivery the way the provider
  does and asserts 200 / 400 / 401 / 409 for valid / missing-signature /
  wrong-signature / replayed deliveries — passing on first run with no manual
  edits beyond the handler bodies and the secret env var. `--path`,
  `--secret-env`, `--dry-run`, and `autumn destroy webhook` are all supported;
  a second endpoint on a path another endpoint already claims is refused at
  generate time rather than failing config validation at boot, regenerating with
  a changed path updates the endpoint block in place instead of stranding it, and
  `destroy` leaves hand-edited config (rotation variables, a Redis replay
  backend) alone — and recovers a generation-time `--path`/`--secret-env` from
  the recorded endpoint block, so cleanup does not depend on repeating flags.
  See `docs/guide/generators.md`.

- **`autumn webhook sim` refreshes body-carried delivery IDs (#1366):** the
  simulator already minted a fresh delivery ID per invocation for GitHub and
  generic providers, which carry it in a header. Stripe and Slack read theirs
  from the JSON body (`id` / `event_id`), so a payload with a fixed ID replayed:
  the first simulation was accepted and every one after it answered `409
  Conflict` for the length of the replay window. Both fields are now rewritten
  before signing (the signature covers the exact bytes sent), and the substituted
  ID is printed. A payload that is not a JSON object is left exactly as written.

- **`autumn webhook sim --event <TYPE>` (#1366):** the simulator hardcoded
  `sim.event` as the announced event type for the header-carrying providers
  (`X-GitHub-Event`, `X-Webhook-Event`), which matches no real dispatch arm — so
  a simulated delivery always fell through to a handler's
  acknowledge-and-ignore branch, proving nothing about the code under test.
  `--event` now sets it (default unchanged), and `autumn generate webhook` prints
  a matching flag for those presets. Stripe and Slack read their event type from
  the payload's `type` field, so `--event` warns rather than silently doing
  nothing there. A `409 Conflict` response now explains which replay key
  rejected the delivery, and an HTML error page is summarized instead of dumped.
- **Edge WASM capsule, first slice (#1790):** mark a read-path handler
  `#[edge]`, register it with `edge_routes![]`, and a single `autumn build`
  emits a portable `wasm32-wasip1` **edge capsule** alongside the native binary
  — one codebase, no vendor SDK, no rewrite. The capsule runs the same `axum`
  router over the same handler source at the CDN edge; the origin binary stays
  the authority and still mounts every edge route, so anything the edge cannot
  serve — a write method, an unknown path, an unmediated capability, a panic —
  becomes a typed `fallthrough` the host forwards upstream, with no
  author-written glue. One platform seam is mediated: `EdgeCache` over the new
  `EdgeKv` trait reads the app's own cache at the origin
  (`AppBuilder::with_edge_kv`, the new non-default `edge` feature) and performs
  a host round trip at the edge, so `#[edge(needs(kv))]` handlers compile
  unchanged for both substrates. Refusals are compile-time and actionable: a
  native-only extractor fails the `EdgeHandler` bound with a diagnostic naming
  the fix, and `#[edge]` on a non-GET route or alongside
  `#[secured]`/`#[authorize]`/`#[step_up]`/`#[throttle]`/`#[static_get]` is a
  spanned error. `autumn doctor` gains `edge_target` and `edge_routes`. The
  byte-identity claim is proven, not asserted: `examples/edge-greeting` drives
  one request corpus through the native lane, a real wasm artifact and the full
  origin app in the new unfiltered `edge-conformance` CI job. The
  `autumn-edge` crate, its wire protocol and its host API are **experimental**.
  See `docs/guide/edge.md`.

- **Ordered-list `position` field with transaction-safe reorder helpers
  (#1358):** a new `position` DSL token (`rank:position`, or scoped to a
  parent with `rank:position{scope:board_id}`) declares a user-orderable
  list — todo priorities, kanban columns, playlist tracks — with zero
  hand-written reindexing SQL. The column is server-managed (`#[position]`,
  excluded from `New*`/`Update*`): database triggers assign the next
  contiguous value on insert and compact the remaining rows on delete (or
  soft-delete), so the `0..len-1` invariant holds for every insert/delete
  path, not just the generated repository's. `#[repository(Model,
  position(column = "rank"[, scope = "board_id"]))]` generates
  transaction-safe `move_to(id, n)` / `move_before(id, other)` /
  `move_after(id, other)` / `move_up(id)` / `move_down(id)` methods, each
  `O(rows shifted)`, locking the scope's rows (ordered by `id`, a fixed
  lock order that serializes concurrent movers on the same scope instead of
  deadlocking them) before shifting. The HTML scaffold's index orders by
  the position column and adds no-JS, CSRF-protected Move up/down buttons.
  Not yet supported with `tenant_scoped`/`versioned`/`dependent(...)`/
  `sharded` repositories (refused at macro-expansion time, matching
  `retention(...)`'s posture on the same combinations). See
  `docs/guide/generators.md`.

- **Codemods with `autumn upgrade` (#1629):** the new `autumn upgrade` command
  applies each release's *mechanical* app-code migrations to your own Rust
  source. For every release between the `autumn-web` version your `Cargo.toml`
  records and the target, it rewrites that release's machine-applyable changes
  — first shipped: 0.6.0's `with_pool` → `with_pool_untracked` repository
  constructor rename. It writes nothing by default: a bare `autumn upgrade`
  prints a per-file diff plus a count of affected sites, and `--apply` is the
  explicit write step. Rewrites match whole tokens through a token-level parse
  rather than a text substitution, so string literals, comments, same-named
  locals and `with_pool_provider` are untouched, formatting and comments
  survive byte-for-byte, and a second run is a no-op. Anything the tool cannot
  safely reach — a call site inside a macro invocation or an attribute — is
  reported with `file:line` under `manual` with a link to the guide section,
  never guessed at. Every documented breaking change now carries an
  `**Automation:**` confidence label (`auto` / `review` / `manual`) in its
  migration guide, and `scripts/check-migration-guides.sh` fails a rename-level
  break that has neither a shipped codemod nor a stated reason for staying
  manual. See `docs/guide/upgrading.md`.
  <!-- migration-guide-gate: describes tooling for breaking changes; the
  command and the gate are both additive -->

- **Declarative data-retention sweeps (#1342):** `#[repository(Model,
  retention(after = "30d", basis = created_at))]` — and the soft-delete
  `purge_deleted_after = "90d"` variant, composable with `after` — compiles to
  a batched, cursor-paginated, fleet-coordinated sweep with no
  `#[scheduled]` fn and no SQL. Age-based retention soft-deletes on a
  `soft_delete` repository (never re-touching an already soft-deleted row)
  and hard-deletes otherwise; `purge_deleted_after` always hard-purges,
  re-checking `deleted_at` at delete time so a row `restore()`d between the
  sweep's SELECT and DELETE survives. Every policy auto-registers via
  `inventory` — no `tasks![...]` entry needed — and reuses the same
  Postgres-advisory-lock scheduler coordination `#[scheduled(coordination =
  "fleet")]` tasks get, so only one replica executes a given sweep per
  interval. `autumn retention --dry-run [--model NAME]` reports per-model
  rows-that-would-be-swept without deleting anything. Each real run emits a
  structured log line and bumps `retention_sweep_rows_total` /
  `retention_sweep_duration_seconds`, both labeled by `model`. A
  counter-cached model (`#[belongs_to(..., counter_cache)]`) moves its
  parent's counter on every sweep, exactly like `delete_many` — the sweep
  locks the still-eligible rows before mutating them, so a counter-cache
  decrement and the corresponding delete/soft-delete always agree on the
  same set. Opt-in: a repository with no `retention(...)` behaves exactly as
  before. See `docs/guide/retention-sweeps.md` and the `examples/saas`
  `PasswordResetToken` demo.

- **security:** `autumn routes audit` now proves *what* a route is authorized
  to do, not merely that it is guarded. The security posture manifest moves to
  schema v3 and gains a fourth dimension, `authorization_policies`
  (`provenance: "provable"`, `source: "macro:#[authorize]"`): one
  `(action, resource)` entry per `#[authorize]` binding, recovered from the
  macro expansion in either attribute order and through stacked guards, then
  sorted by `(path, method, action, resource)` and deduplicated so a rebuild
  of unchanged code stays byte-identical. The dimension carries a required
  `runtime_caveat` naming the one step a build cannot prove — which
  `impl Policy<R>` the `PolicyRegistry` serves at boot — rather than shipping
  a bare `provable` tag it cannot defend. `authorization_policies` therefore
  leaves the `excluded` list; `policy_registration` stays there for the
  boot-time fact, reworded to point at that caveat, and a new
  `repository_policy_bindings` entry discloses that
  `#[repository(api = ..., policy = ...)]` auto-APIs set
  `routes.entries[].policy` without leaving a binding the macro can recover
  (`routes.entries[].policy` is a superset of the proven bindings, and stays
  one). Carrying the bindings adds two pieces of route metadata:
  `ApiDoc::authorize_bindings` (compile-time `&'static [AuthorizeBinding]`)
  and its `RouteInfo::authorize_bindings` wire twin
  (`Vec<AuthorizeBindingInfo>`, elided from the routes dump when empty, so
  older dumps still deserialize unchanged). The recorded resource is the
  identifier as written, never the `Policy` impl. The provenance rubric
  deciding when a dimension may claim `provable`, `declared`, or
  `runtime-only` — and when a mostly-provable dimension ships with a caveat
  instead of a demotion, with outbound HTTP worked through as `declared` — is
  now documented in `docs/guide/security-posture-manifest.md` (#1627).

- **`autumn generate scaffold --i18n` emits translatable views** (issue #1349).
  Autumn already shipped the whole Fluent stack — the `t!(locale, "key")` macro
  with compile-time key validation, the `i18n/<tag>.ftl` convention, fallback
  chains, and an `Accept-Language` `Locale` extractor — but the scaffold emitted
  hardcoded English into every view, so localizing a generated resource meant
  hand-replacing roughly a dozen strings and hand-authoring the matching keys,
  per resource. With the new flag: every user-facing string in the generated
  views (page titles, `h1` headings, buttons, links, index column headers,
  show-page property labels, form control labels, enum options, empty-state
  copy, the delete-confirm prompt, the one-shot flash notices, the media
  type/size line beside a stored attachment, the duplicate-value error a
  `unique` column raises, the optimistic-lock conflict banner, the flash a
  refused state transition shows, and the labels
  the shared pager/bulk-delete/confirm widgets supply by default) is emitted as
  a `t!` lookup; each view-rendering handler takes the `Locale` extractor as its
  first parameter; and the default locale's bundle is created — or merged into,
  preserving values you have already translated — with every key the views
  reference and only those, so `autumn i18n check --strict` passes on the
  result. Keys land in the bundle the app actually reads — a project on
  `default_locale = "fr"` with `dir = "translations"` gets
  `translations/fr.ftl`. The project is
  wired so those lookups resolve with no further config: autumn-web's `i18n`
  feature is enabled, `[i18n] default_locale = "en"` is added to `autumn.toml`
  when it has no `[i18n]` section, `.i18n_auto()` goes into the `AppBuilder`
  chain (found with a comment- and string-aware scan, so it can never be written
  into a comment), and the generated `Dockerfile` copies `i18n/` into both
  stages — without it `.i18n_auto()` panics at startup in the container. Shared
  chrome (`common.create`/`save`/`back`/`edit`/`delete`/`show`, plus the widget
  defaults) is written once per project and reused across resources rather than
  duplicated per model. Row keys and counts interpolate as Fluent arguments so a
  translation can position them; the model's **name** never does — "New Post" is
  a per-resource key, because a noun dropped into a sentence pattern cannot be
  made to agree in gender or case from the bundle alone. Composes with
  `--searchable`, `--soft-delete`, `--sharded`, and the CSV export, whose added
  strings are translated too; `--api` renders no labels, so the flag is a no-op
  there and writes no `.ftl`. Refused with `--live`, `--live-validation`, and
  `--belongs-to`, whose views render outside a request or inside the parent
  resource's handler, and for a resource named `Common`, whose keys would
  collide with the shared chrome namespace. `autumn destroy scaffold … --i18n`
  takes back that resource's marked block (including continuation lines a
  translator wrapped a value over, and nothing outside it, so hand-authored
  keys on the same prefix survive) and drops the shared chrome once the last `--i18n`
  resource is gone. **Without `--i18n`, scaffold output is byte-for-byte
  unchanged.** One caveat: with the flag on, a single key per field labels the
  index header, the show row, and the form control alike, so a *multi-word*
  column's show-page label normalizes from "Author name" to "Author Name"
  (single-word columns are unaffected). Validation messages — the
  `UNIQUE_CONSTRAINTS` table's "has already been taken" — stay English, matching
  the issue's out-of-scope list.

- **`json`/`jsonb` scaffold field type (#1341):** `autumn generate scaffold Setting config:json` (or `config:jsonb`, `Json`, `Jsonb` — like `Attachment`/`attachment`, both the lowercase and PascalCase spelling of each alias are accepted) adds a `config` field whose model type is bare `serde_json::Value` — no wrapper struct — mapped to a Postgres `JSONB` column (`Nullable<Jsonb>` for `Option<json>`), matching loco's `... data:jsonb` in a single command. Unlike the existing JSONB-backed `Attachment` field (`autumn_web::storage::Blob`), which needed hand-written `FromSql`/`ToSql` because `Blob` is a local type, `json`/`jsonb` needs **zero** new `autumn-web` conversion code: diesel itself already implements `FromSql`/`ToSql<Jsonb, Pg>` for `serde_json::Value`, and — on the `SQLite` dev/test backend — `FromSql`/`ToSql<Json, Sqlite>` too (diesel 2.3+, behind the `serde_json`/`sqlite` cargo features this workspace already turns on unconditionally). The `SQLite` column is `TEXT` via diesel's `Json` sql-type specifically, not its `Jsonb` sql-type, which uses a proprietary binary encoding rather than plain-text JSON.

  The generated `<textarea>`-based create/edit form parses the submitted text as JSON in `into_new`, the same `serde_json::Value: FromStr` + `AutumnError::bad_request_msg` pattern already used for `Decimal`/`Uuid`/enum fields — invalid JSON is a 400, not a 500, and a blank *optional* textarea means "no value" rather than a parse failure. The auto-generated JSON API round-trips the field as a native object/array for free (`serde_json::Value: Serialize`/`Deserialize` as itself). CSV export wraps it in the existing formula-injection guard defensively; it is excluded from server-side column sorting (same as `Attachment`/`Enum`/`Bytea` — `serde_json::Value` isn't in the `#[model]` macro's orderable allowlist) and, like `Text`/`RichText`, hidden from the admin's default list view — the admin panel already had a purpose-built `AdminFieldKind::Json` (monospace textarea + submit-time JSON coercion) that the new DSL token now reaches. That coercion step now rejects malformed non-blank JSON with a `400` instead of silently persisting the raw unparsed string (a pre-existing gap in `autumn-admin-plugin`'s form handling, hardened alongside this since the new DSL wiring made it newly reachable). `--default` is supported (`config:json={}`), validating the literal is syntactically valid JSON before quoting it verbatim into the migration's `DEFAULT` clause. [no-plugin]

- **`{encrypted}` field-DSL modifier:** `autumn generate scaffold`/`model` can
  now declare an at-rest encrypted column in one token —
  `'api_token:String{encrypted}'` emits `#[encrypted]` on the generated model
  field, and `'email:String{encrypted:deterministic}'` emits
  `#[encrypted(deterministic)]` so the column still supports
  `find_by`/`exists_by` equality lookups and a real `UNIQUE` index. The
  migration column is unbounded `TEXT` — sized for the base64 ciphertext
  envelope, not the plaintext — with a comment saying so, and the admin
  generator's existing auto-detection picks the attribute up end to end, so a
  scaffolded encrypted column is redacted in the admin with no extra flags.
  Previously the only way to encrypt a scaffolded column was to remember to
  hand-edit the generated model, and forgetting shipped plaintext silently.
  Because randomized ciphertext can never satisfy an equality predicate, the
  generator now refuses at generate time — pointing at
  `{encrypted:deterministic}` as the fix — the combinations that would
  otherwise fail at runtime with `EncryptionError::RandomizedEqualityLookup`:
  `:unique`/`--unique`, `--query`, and `--index`. A second set of combinations
  is refused in *both* modes, because no mode makes them work: `--searchable`
  (full-text search indexes the stored ciphertext), `--default` (a defaulted
  column bypasses the encrypting insert), `--shard-key` (the shard is chosen by
  hashing the stored value), `Option<…>` and non-text field kinds (the v1
  attribute is non-null `String` only), a `:states(…)` state machine (the
  transition handler's raw write would bypass encryption), and deriving a
  `slug{from:…}` from an encrypted column (a slug is stored in its own
  plaintext column *and* used as the record's URL). `generate admin` refuses a
  `{encrypted}` token the on-disk model does not back with the attribute:
  redacting a column the model stores in plaintext would manufacture a false
  at-rest guarantee rather than provide one.

  The generated app's own surfaces follow suit: the index table renders
  `••••••••` with no sort link, the CSV export omits the column entirely (as
  the admin panel's export already did), and the admin no longer offers to sort
  by it. The `show` view and `edit` form still render the value — you routed to
  one record deliberately, and a form has to show what it is editing.
  `generate` also prints the `autumn credentials edit` next step naming the
  exact key material the new column needs.

- **`MarkdownRegistry::static_params_for(param)`:** derive SSG params for a
  `#[static_get]` route whose path parameter is not named `slug` — e.g.
  `#[static_get("/docs/{page}", params = …)]` needs
  `static_params_for("page")`, because a `StaticParams` entry keyed `"slug"`
  leaves `{page}` unsubstituted. `static_params()` is unchanged and now
  delegates to it with `"slug"`.

### Performance

- **`Changeset::field_value` no longer re-serializes the whole record per
  field:** every scaffolded form-rendering helper that populates a value
  (`text_input`, `textarea_input`, `number_input`, `date_input`, and their
  `required_*`/`*_htmx` variants) calls `Changeset::field_value` once to read
  a single field back out of the changeset's data. `field_value` ran
  `serde_json::to_value(&self.data)` — serializing every field of the record
  — on **every** call, so a form with N value-bearing fields paid a full
  record serialization N times over on each render. `Changeset<T>` now caches
  the serialization in a private `OnceLock<Box<serde_json::Value>>`, computed
  once on the first `field_value` call and reused by every later call on the
  same changeset. No public API change: `field_value`'s signature and
  behaviour are unchanged, including returning `None` on a serialization
  error (cached as `Value::Null`, whose `.get()` returns `None` exactly as
  the old early-return did).

  Measured with a new benchmark (`autumn/benches/form_render.rs`, a realistic
  12-field scaffolded form with two fields carrying validation errors, a
  fresh `Changeset` built per render so the cache is never pre-warmed across
  iterations — a real request builds one `Changeset` and renders it once —
  2,000 iterations = 2,050 renders), `valgrind --tool=callgrind` and
  `--tool=dhat`, before and after on the same machine:

  | | before | after | delta |
  | --- | ---: | ---: | ---: |
  | Instructions (5,000-iteration run) | 932,490,618 | 364,082,814 | **-61.0%** |
  | Allocation bytes/render (marginal) | 45,003 | 21,579 | **-52.0%** |
  | Allocation blocks/render (marginal) | 364 | 124 | **-65.9%** |

  `serde_json::SerializeMap::serialize_field` drops from 13.18% of
  instructions to 3.07% — consistent with the 11 value-reading fields on the
  benchmark's form collapsing to one real serialization per render instead of
  11 — and the `BTreeMap` machinery backing `serde_json::Map` mostly drops out
  of the top-90%-of-instructions list; `maud::escape_to_string`'s absolute
  instruction count is unchanged (109,276,950 both before and after),
  confirming the win is isolated to the redundant serialization and doesn't
  touch the genuinely
  inherent HTML-escaping cost.

- **ingress middleware no longer boxes a future per layer per request:** every
  `axum::middleware::from_fn` on the framework's always-on request path is now a
  hand-rolled `tower::Service` with a **named** future. `from_fn` cannot avoid
  the cost it was paying: the async block it generates has no nameable type, so
  `FromFn::call` returns it as `Box::pin(..)` — one heap allocation per call
  site per request, sized by everything that block captures across its single
  `.await`, which for an outer layer is the whole downstream continuation. DHAT
  measured those boxes at **19.57% of every byte** the `request_pipeline`
  benchmark allocated (5,267,600 of 26,918,238 bytes over 650 requests) while
  being only 2.14% of the blocks — the largest single allocation cost in the
  profile (issue #2214).

  Converted: the asset cache-control layer, the event-bus app context, the
  webhook replay-key cleanup, the method-override rejection filter, the
  trusted-host gate, the startup barrier, the per-request timeout, the
  read-your-own-writes pin, and (under `oauth2`) the HTTP-interceptor scope.
  Each keeps its behaviour and its position in the stack exactly; what goes away
  is the box and, with it, the `self.inner.clone()` `from_fn` needed to move the
  inner service into that box — which for an erased `BoxCloneSyncService` was a
  recursive `clone_box` down the rest of the stack, so each conversion took a
  whole deep-clone cascade with it too.

  Re-run of the issue's own DHAT recipe on `benches/request_pipeline.rs`
  (release, 200 iterations = 650 requests), before and after on the same
  machine — filtering allocation sites whose second stack frame is
  `FromFn<..>::call` exactly, as the issue specifies:

  | | `FromFn::call` `Box::pin` | share of run bytes | marginal blocks/req | marginal bytes/req |
  | --- | ---: | ---: | ---: | ---: |
  | before | 3,250 blocks / 5,215,600 bytes | 19.80% | 168.8 | 36,826 |
  | after | **0 / 0** (0 sites) | **0%** | 139.2 | 25,943 |

  The before column reproduces the issue's measurement exactly on block count
  (3,250) and to within 1% on bytes. Overall: **−17.5% allocations** and
  **−29.6% bytes** per request.

  The same movement is pinned as a regression gate in the debug profile, where
  it is deterministic run to run: **172 → 140 allocation blocks** and
  **37,819 → 26,030 bytes** per request under the default feature set. The ingress clone-on-call traversal count drops from 13 to 9 in
  the same move, on the default set and on a 13-feature build alike. One layer
  also sheds an allocation of its own: the asset cache-control layer no longer
  clones the request path into a `String` on every request in the app.

  Two middlewares are deliberately **not** converted, because they `.await`
  before calling the inner service and their futures therefore cannot be named
  without `type_alias_impl_trait`: the tenancy middleware (async tenant
  extraction) and the rate-limit principal shim (async session read). Both are
  off by default. `webhook_replay_cleanup` keeps one box, taken only on a `5xx`
  that actually registered replay keys; on every other request its future is
  unboxed (it still mints the per-request replay cell it always did).

  One public behaviour change: `read_your_writes::middleware` used to
  `unreachable!()` when handed `ReadYourWrites::Off`. It now debug-asserts and
  falls back to an inert `Off` pin instead of panicking on the request path.
  Both call sites gate on `mode != Off`, so the arm stays unreachable in
  practice. Otherwise nothing public moved: `asset_cache_control`,
  `method_override_rejection_filter` and `webhook_replay_cleanup_middleware`
  remain exported `async fn`s with identical behaviour, now sharing their
  decision logic with the layers so the two forms cannot drift.

  Four gates keep the win from eroding: a per-request allocation **blocks**
  ceiling tight enough that restoring a single `from_fn` fails it, a companion
  **bytes** ceiling derived under the wider feature set CI actually gates with,
  the ingress traversal count pinned to its exact measurement, and a
  `type_name`-based assertion that none of the converted services ever returns a
  `Pin<Box<dyn Future>>` again.

- **config reads on the request path:** generated auth handlers, the admin
  plugin, the `saas` starter, and the `blog`/`saas`/`teams` examples now read
  configuration through `AppState::config_arc()` instead of
  `AppState::config()`. `config()` returns an owned `AutumnConfig`, so every
  call deep-clones **every** config section — 64 allocations and 1,384 bytes
  against a default config, and more as an app's config grows — even to read a
  single `bool` or `usize`. On a handler that cost is paid per request, and a
  handler reading two or three sections paid it two or three times over: a
  downstream app profiling its request path measured whole-config clones at
  ~30% of its per-request allocations and ~42% of its per-request bytes.
  `config_arc()` hands back the shared `Arc<AutumnConfig>` the state already
  holds, so the same read is a refcount bump and handlers borrow the section
  they need off the handle (`&config.auth.password`).

  Nothing about the framework's own ingress path changed — that was already
  allocation-free as of #2199 — and no public signature moved: `config()`
  remains the per-boot owned-snapshot accessor, and the one generated call site
  that still uses it is the boot-time `remember_me_startup` hook, which needs an
  owned `RememberConfig`. Apps calling `state.config()` in their own handlers
  keep compiling; switching them to `state.config_arc()` is the fix, and
  `docs/guide/authentication.md` now teaches that as the default. A new
  generator test pins the emitted handlers to `config_arc()` so the deep clone
  cannot reappear in scaffolded apps.

- **jobs:** the Postgres job worker's claim query (`SELECT … FOR UPDATE SKIP
  LOCKED`) no longer scans and sorts the entire ready backlog for a queue
  before picking one row, for apps that don't configure `[jobs] queues`
  priority (the common case: a single `"default"` queue). The claim query's
  `ORDER BY array_position($2::text[], candidate.queue), candidate.run_at`
  was opaque to the planner — `array_position` depends on the bound queue-
  order array, so even though it's constant across every candidate row when
  only one queue is in play, Postgres couldn't prove that and fell back to a
  `Bitmap Heap Scan` of the whole ready-in-queue backlog followed by a
  `Sort` and `LockRows` over every one of those rows, before `LIMIT 1`
  picked the winner. Single-queue workers now send a query that drops
  `array_position` from `ORDER BY` and uses `queue = $2` (scalar), which
  lets the planner recognize the existing `idx_autumn_jobs_queue_ready
  (queue, run_at)` index order and do a plain `Index Scan` + `Limit 1`
  instead. Measured (`EXPLAIN (ANALYZE, BUFFERS)`, production-shaped
  fixture): 703→21 buffers at 4.4k ready rows, 3,342→22 at 44.6k, and
  57,093→22 (eliminating an external-merge sort spill to disk) at 444k;
  workload-level (`pg_stat_statements`, 50 claims) 166,437→1,410 total
  buffers, a 99.15% reduction. No index or migration changes — see
  `docs/reports/2026-08-14-ledger-job-claim-single-queue/`.

- **state:** `AppState::profile` and `AppState::auth_session_key` no longer
  deep-clone a `String` on every `AppState::clone()`. `AppState` is cloned
  once per hop of the ingress tower stack (`Route::call` deep-clones the
  boxed service beneath it, per #2193/#2198), so the two fields still held
  as an owned `Option<String>`/`String` — rather than shared behind an
  `Arc` like the rest of the struct — paid a fresh heap allocation on every
  one of those clones instead of once per request. Both now live behind an
  `Arc<str>`; `profile()` and `auth_session_key()` are unchanged (`&str`
  via `Deref`), and `with_profile`/`with_auth_session_key` still take
  `impl Into<String>`. Measured with the debug-profile allocation-counter
  gate already used for #2198's `config_arc` work (`autumn/tests/config_alloc_gate.rs`):
  a `TestClient` request drops from 220 to 172 allocation blocks (-22%),
  identical across repeated runs.
- **mail:** list-mail sends (`Mailer::send` with `list_unsubscribe` set) now
  resolve suppression for the whole recipient batch in one query instead of
  one `SELECT` per recipient. The `SuppressionStore` trait gained a batched
  `is_suppressed_many` method (default implementation loops over
  `is_suppressed`, so existing custom stores keep working unchanged);
  `DbSuppressionStore` overrides it with a single `WHERE list_id = $1 AND
  subscriber = ANY($2)` query, chunked at 50,000 recipients on Postgres (a
  backstop against an unbounded single-statement bind, not a tuning knob for
  ordinary sends: a tighter chunk size can land statements past a planner
  cost crossover where `= ANY(...)` stops using the index and falls back to
  a table scan, then re-pay that scan's fixed cost once per chunk) and at
  `repository::MAX_BIND_PARAMS - 1` on SQLite, which binds `eq_any` as one
  parameter per element instead of Postgres's single array parameter.
  Measured
  (`pg_stat_statements`, production-shaped `mail_unsubscribes` fixture):
  statement count per send drops from N to 1 at every batch size tested
  (200/2,000/20,000 recipients); total buffers 660→604 (200), 6,600→6,004
  (2,000), and 66,000→8,070 (20,000, −87.8%). No index or migration changes
  — see `docs/reports/2026-08-15-ledger-mail-suppression-batch/`.
- **scaffold:** the generated `index` page for a `belongs_to`/`references`
  field with a resolved display column (#1146) no longer scans the *entire*
  referenced table to label the ~20 rows on one page. `autumn-cli`'s
  `render_index_reference_label_loads` reused the create/edit form's
  `{name}_select_options` loader — a full, unfiltered `SELECT id, col FROM
  table ORDER BY id` that genuinely needs every row for a `<select>` — to
  build the index's parent-label map too, so every index page view re-read
  the whole referenced table regardless of page size. It now scopes the
  query to `WHERE id = ANY(...)` the page's own FK values
  (`page_data.content`, already fetched), and the identical fix applies to
  the `--belongs-to` nested list (`children_section_with`). Measured
  (`EXPLAIN (ANALYZE, BUFFERS)` + `pg_stat_statements`, production-shaped
  fixture): rows read at the scan node drop from 500,000→20 (-99.996%) at
  500k parent rows, with total buffers 7,051→83 (-98.8%); 707→61 (-91.4%)
  at 50k rows and 72→54 (-25.0%) at 5k rows. No index or migration changes
  — see `docs/reports/2026-08-16-ledger-scaffold-index-label-scope/`.

### Added

- **cluster:** an embedded, zero-dependency self-clustering substrate (#1762).
  Two instances of the same binary, started with a shared secret and no
  external coordination service — no Redis, no Postgres, no etcd — discover
  each other over authenticated TCP gossip, report a converged two-member
  view, and expose the first shared distributed primitive: a cluster-wide
  grow-only CRDT counter (`ClusterHandle::counter(name)`, `increment()` /
  `get()`) whose cells are keyed by `(node id, boot incarnation)` and merged
  by per-cell maximum, so an increment on one node is observable on the other
  within a push interval and a restarted node can never undercount. One
  periodic signed state push doubles as the heartbeat: membership is itself a
  CRDT (`Alive`/`Left` records with SWIM-style incarnation refutation), while
  `Suspect`/`Down` live in a purely local liveness overlay — a clean shutdown
  sends a bounded best-effort departure (the node's final document, so an
  increment accepted while requests were still draining is not lost, plus the
  leave notice), and a kill converges by suspicion timeout, after which the
  surviving node keeps serving the counter and reports a one-member view that
  is `UP`, never `DOWN`. Frames are
  HMAC-SHA256-authenticated (cluster name, sender, incarnation, and sequence
  are MAC-covered; constant-time verify before any payload parse; per-sender
  replay watermarks) but not encrypted — deploy the cluster port on a trusted
  network. What an *unauthenticated* socket can cost is bounded too: inbound
  connections are capped, one that delivers no complete frame within its idle
  deadline is closed, and one whose frames keep being refused before they
  authenticate is closed on the third — so the connection cap is a budget only
  peers that prove the secret can hold. A handler that increments on every
  request has its prompt pushes rate-limited to a fraction of the push interval
  rather than gossiping the whole document per write. Opt-in via the new `[cluster]` config
  section
  (`AUTUMN_CLUSTER__*` env forms, fail-fast validation, secret required),
  surfaced through the `cluster:membership` health indicator and
  `autumn_cluster_*` metric families, and implemented with zero new crate
  dependencies. See `docs/guide/clustering.md` for the wire format, failure
  semantics, and a two-terminal walkthrough.
- **actuator:** `HealthIndicatorRegistry::contains` and
  `MetricsSourceRegistry::contains` report whether a component name is already
  taken (#1762). Neither registry can unregister, so a subsystem that claims a
  name in both — as the cluster does — can now check first and fail without
  stranding half of itself behind an error.
- **generate scaffold:** `--soft-delete` now also generates the recover-from-
  trash UI its data layer has been waiting for (#1332), finishing #689's AC6. A
  standard HTML scaffold gains a `#[secured] GET /<plural>/trash` page that lists
  deleted rows through the repository's generated `page_only_deleted` (the
  paginated `only_deleted` scope — the list handler writes no `deleted_at`
  filter of its own), a **Trash** link in the index's page furniture, a
  `Deleted` column carrying each row's `deleted_at` stamp, and per row a
  CSRF-protected **Restore** (`POST /<plural>/{id}/restore` → `restore(id)`) and
  **Purge** (`POST /<plural>/{id}/purge` → `purge(id)`) control, the latter
  behind `confirm_action`'s server-rendered dialog (titled per row, so a
  page-sized trash does not stack identical headings and the person confirming
  an irreversible delete can see which record it is) rather than an inline
  `onclick` the default `script-src 'self'` CSP would block. The delete button's
  flash becomes `<Model> moved to Trash` — with somewhere to recover from, the
  flash is the only thing that says so — and a derived slug of `trash` joins
  `new`/`search` as a reserved segment. A `--soft-delete` scaffold on one of the
  gated-off variants warns at generation time, naming the reason, instead of
  silently shipping no recovery UI. Both write handlers
  load their target with `deleted_at IS NOT NULL` first, so they 404 rather than
  hard-deleting a live row, and record-authorize it with the same `"delete"`
  action `destroy` uses. The generated `tests/<name>.rs` gains an in-process
  lifecycle test (create → soft delete → present in Trash and absent from the
  index → restore → back in the index → purge → gone from both). No new
  `autumn-web` API: the scaffold consumes the existing macro methods and the
  shipped CSRF/flash/widget helpers. Emitted **only** under `--soft-delete` and
  only on the standard HTML path — `--live`/`--live-validation` (a restore does
  not broadcast), `--sharded` (`page_only_deleted` cannot fan out), an
  owner-scoped index (no owner-filtered deleted-rows scope to read through), and
  `--api` stay byte-identical to their prior output, as does every scaffold
  generated without the flag. See `docs/guide/generators.md`.

- **failure capsules:** a failing request can now be recorded and replayed
  offline (#1598). With `[failure_capture] enabled = true`, every caught panic
  and every 5xx writes a **capsule** — one JSON file holding the redacted
  request, the database traffic the handler produced, the clock readings it
  took and the outcome the client received — and `autumn replay <capsule>`
  rebuilds the application around it and re-runs it. The gap this closes is the
  one between "I have a stack trace" and "I can reproduce it": a production
  500 that depends on the row a query returned is not reproducible from a log
  line, however structured, and the usual answer — copy the request into a
  test, guess at the data — is exactly the guess that makes a bug take a week.

  Database effects are captured at the **wire**, not at the query API. A pooled
  connection is established through a tee that frames PostgreSQL protocol
  messages in both directions and groups them into exchanges: the SQL, the bind
  parameters, and the raw backend frames — `RowDescription`, every `DataRow`,
  `CommandComplete`, `ReadyForQuery` — byte for byte. Replay serves those bytes
  back to a real `tokio-postgres` client through an in-process stub server over
  an in-memory duplex pipe, so no socket is opened and no live database is
  contacted. The wire is the only honest seam available: diesel's `PgRow` wraps
  a `tokio_postgres::Row` that has no public constructor, so rows cannot be
  fabricated at the API level by anything — including Autumn. Attribution costs
  no extra latency: `Db::checkout` merges `SET autumn.capsule_request` into the
  same round trip as `SET statement_timeout`, and a checkout with no capture
  scope sends the clearing form so background work can never land in whoever
  held that connection last. Each capsule also carries the connection's memo —
  its session prologue, its already-prepared statement metadata, its catalog
  lookups — because a capsule recorded on a warm pooled connection must replay
  against a cold stub.

  **A capsule contains real request data and real database rows**, and the
  documentation leads with that rather than burying it. Headers, query
  parameters and structured bodies are masked through the same
  `[log] filter_parameters` list the access log uses (plus every `#[encrypted]`
  column name); any SQL bind whose bytes echo a masked value is blanked, and so
  is any masked value quoted back inside an outcome message, panic payload or
  backtrace. What is *not* masked is stated just as plainly: database result
  rows, URL path segments, unstructured bodies, the SQL statement text as sent
  (rewriting it would break the key replay matches tapes on, so a value your
  code interpolates instead of binding lands in the capsule) and the raw
  backend `ErrorResponse` frames, which PostgreSQL fills with the offending row
  values. Filter matching is by equality after normalization, so `x-api-key`,
  `proxy-authorization` and `x-auth-token` do *not* match the built-in
  `api_key`/`authorization` keys and have to be added to `filter_parameters` —
  the guide gives the table and the snippet. A body that declares structure
  but does not parse as it is dropped entirely rather than recorded unmasked.
  Capsules are written owner-only (0600 on unix) through a temp-then-rename,
  pruned oldest-first before each write, and off by default. `autumn new` now
  gitignores `/tmp/`, where capsules and the other runtime flag files live.

  `autumn replay` compiles the app and runs it in a replay mode that forces
  in-memory sessions, refuses outbound HTTP and channel delivery, and skips
  migrations, storage preflight, the cache backend, the job runtime, the
  scheduler, the mailer and the fail-fast configuration gates — the clock and
  the database come from the capsule, so the only remaining variable is the
  code. Your handlers, extractors and custom layers *do* still run, against the
  bytes in the file, so replay capsules you trust (the guide says this in the
  security section). The verdict is JSON on stdout and a
  human summary on stderr: exit 0 `reproduced` (same status code, message and
  Problem Details type, and no database divergence), exit 1 `mismatch` (the
  tape lined up, the outcome changed — what a fix looks like) or `diverged` (the
  code asked the database something the recording never asked, so the run was
  not a fair comparison), exit 2 `refused` for a truncated capsule, a
  `format_version` this build does not understand, an unreadable file, or a
  PostgreSQL tape handed to a sqlite build. A divergence names the connection,
  the position in its tape and the SQL involved; a 401/403 where the recording
  answered a server error is called out as the redaction limit it is, not left
  as a puzzle.

  Overhead is measured, not asserted. `failure_capsule_overhead.rs` drives
  2 000 requests per phase in interleaved rounds against a local PostgreSQL 16
  and prints both numbers that matter: on a route that does nothing else,
  capture costs +55 µs p50 / +76 µs p95 (479 → 533 µs p50); on a route doing
  one bound `SELECT`, +80 µs p50 / +62 µs p95 (1 922 → 2 002 µs p50) — tens of
  microseconds, or 2.6–4.2% of a request that talks to a database once and
  11.5–12.6% of one that does nothing at all. A repeat run put the two p50
  deltas at +43 µs and +128 µs, widening those to roughly 3–7% and 9–13%, which
  is a fair measure of the noise. **The benchmark is serial** — one request in
  flight at a time — so it cannot show contention on the process-wide scope
  registry (locked twice per request and once per checkout); concurrent-load
  cost is not characterized. Indicative only: CI-class virtualized hardware,
  unoptimized build, database on localhost.

  Slice-1 limitations are enumerated rather than implied: authenticated and
  CSRF-protected routes do not replay faithfully (their credentials are
  masked, so the replay stops at the auth layer); one request per capsule;
  same-commit replay is what is tested, and a framework-version difference
  warns; concurrent connections within one request — including a request handed
  two different connections under pool contention — may diverge on ordering;
  capture needs plaintext TCP PostgreSQL, so `sslmode=require`/`verify-ca`/
  `verify-full` (but not `prefer`, `disable` or an absent `sslmode`), a
  Unix-socket URL, a sqlite build, a custom `DatabasePoolProvider` or a shard
  pool disables the database tape (the capsule says so in its notes);
  `LISTEN`/`NOTIFY` is unsupported on capture-enabled request pools; `COPY`
  streams mark the capsule truncated, as do 10 000 clock readings, 64
  exchanges in flight on one connection and an 8 MiB protocol frame; and
  successful requests are never captured. `ErrorEvent` gains a
  `capsule` field whose file is already on disk by the time a reporter runs.
  See `docs/guide/failure-capsules.md`.
- **counter caches:** `#[belongs_to(Post, counter_cache)]` keeps a
  `posts.comment_count` column current automatically (#1325). Creating a child
  increments the parent, destroying one decrements it, soft-deleting decrements
  and `restore` puts it back, and reassigning the foreign key moves the count
  from the old parent to the new one — every one of them **inside the same
  transaction as the row mutation**, so the column and the row commit or roll
  back together. The arithmetic is a single atomic `UPDATE posts SET
  comment_count = comment_count + $1`, never a read-modify-write, so N
  concurrent inserts yield exactly N. The column name defaults to
  `{snake(child)}_count` and takes an override (`counter_cache =
  "subscriber_count"`); a counter on a `has_many`, on a `through =` join table,
  or two legs resolving onto one parent column are directed compile errors. Every
  counter-cached repository also gains `recompute_counter_caches()` /
  `recompute_counter_caches_for(parent_id)` — an idempotent rebuild from the
  source of truth, which is both the backfill for a table adopting the column and
  the repair for drift introduced by writes that bypassed the repository.
  `autumn generate scaffold … --belongs-to Post --counter-cache` emits the
  `BIGINT NOT NULL DEFAULT 0` column and its migration, opts the generated child
  model in, and prints the two parent-side lines (`src/schema.rs`, the model
  field) rather than editing files it does not own and cannot cleanly revert. Models with no counter cache are unaffected: the spec slice is empty
  and the presence flag is a `const false`, so no statement is issued and the
  transaction-free single-statement mutation paths keep their exact prior
  codegen. See `docs/guide/counter-cache.md`.

- **generate scaffold:** `--belongs-to <Parent>` scaffolds the parent-side half
  of a parent → child relationship, which the flat scaffold has always omitted
  (#1323). Autumn already shipped every piece — the `references:` column
  (#1026), the belongs_to `<select>` (#1146), `PageRequest`/`Page`,
  `data_table`, the Changeset re-render (#1124) — but nothing composed them into
  the one view every CRUD app needs: a parent's show page that lists its
  children and lets you add one inline. Our own `examples/reddit-clone` spends
  ~165 hand-written lines on a richer version of exactly that shape.

  ```bash
  autumn generate scaffold Post title:String
  autumn generate scaffold Comment body:Text post:references --belongs-to Post
  ```

  That second command now emits, on top of the usual flat CRUD:

  - `GET /posts/{post_id}/comments` — the child list, scoped to one parent and
    paginated through the existing `PageRequest` extractor. A parent id that
    doesn't exist answers 404 rather than a plausible-looking empty list;
  - `POST /posts/{post_id}/comments` — a `#[secured]` create whose foreign key
    comes from the **path**, never the submitted body (the nested form renders
    no control for it, and the handler overwrites the column before validating,
    so a hand-crafted body cannot re-parent a child). Invalid input re-renders
    at 422 with inline errors and preserved input; success redirects (PRG) to
    the parent's show page;
  - a `pub children_section(…)` helper in the child's routes module — the child
    list (`data_table`, each row linking to the child's own show view) plus the
    inline "add" form — which the **parent's generated `show` view** now renders,
    and which any hand-written page can call too;
  - a back-link from the child's show view to its parent;
  - a generated write-path test that pins the whole point of the nesting: create
    a child under a parent, see it in that parent's list, and see that it does
    **not** appear under a different parent (plus that a body-supplied foreign
    key is ignored).

  The parent-side edit is marker-delimited, so re-running the generator never
  double-injects and `autumn destroy scaffold … --belongs-to Post` takes exactly
  those lines back out — including when one parent has several nested children. Because
  the markers record the relationship durably, `--belongs-to` is a one-time
  flag: a later `generate … --force` without it keeps the nesting (with a
  warning) instead of half-dismantling it, and `destroy` finds the parent
  without it. Re-pointing a nested child at a *different* parent, or dropping
  its foreign key, is refused before anything is written — the parent's section
  passes its own `row.id`, so the change would leave that call compiling while
  reading another table's ids. Un-nest with `destroy` first.
  Regenerating the PARENT re-applies the child sections it carried (and is
  refused when the re-render would reshape `show` into something the section
  cannot live in, such as a `--sharded` parent's `ShardedDb`), and destroying a
  parent that still has nested children is refused with the order to follow.
  The injected signature carries a reversible `#[allow(clippy::too_many_arguments)]`,
  since a generated project's own CI runs `cargo clippy --all-targets -- -D warnings`
  and nine parameters trips it — as the flat `index` and `<snake>_form_for`
  helpers already did, which this also fixes.
  When the child carries an owner column, the nested list inherits the flat
  index's `#[secured]` + owner scoping, so nesting never opens a second, wider
  door onto the same rows.

  Not supported (refused at generation time with an actionable message) with
  `--api`, `--live`, `--live-validation`, `--sharded`, an `Attachment` column, a
  nullable or self-referential parent reference, or a parent that isn't
  scaffolded, is `slug`-keyed, carries a `:states(…)` column, or has a
  hand-rewritten `show` view. Single-level nesting only.
- **sim-testing:** closed the **monotonic gap** — the last raw-`Instant` source of
  nondeterminism inside a `#[sim_test]` (#1797). `ClockSource` modelled only
  `DateTime<Utc>`, so every framework site that measured a *duration* (uptime,
  scheduled-task run times, DB checkout and per-statement timings, job
  uniqueness windows) reached for `std::time::Instant` — and tokio's
  `start_paused` runtime does **not** virtualize `std::time::Instant`, only
  `tokio::time::Instant`. A 24-hour virtual advance read back as microseconds,
  and two runs of the same seed disagreed. The new `time::MonotonicInstant` plus
  `ClockSource::monotonic` are the monotonic twin of `ClockSource::now`: an
  offset from each source's own origin, so a virtual clock can produce one at any
  point while production keeps genuine monotonicity (`SystemClock` derives it
  from a process-global `Instant`, so an NTP step can never make an elapsed
  duration negative). `monotonic()` ships with a **default body**, so an existing
  downstream `impl ClockSource` keeps compiling with byte-identical behavior.
  Reach it from a handler via the `Clock` extractor's new `.monotonic()`, from
  framework internals via `AppState::monotonic()`, and — where no clock is
  reachable at all — via `time::monotonic_now()`. `AppState::uptime()` /
  `uptime_display()` keep their exact signatures. This is a *seam* migration, not
  full coverage: CONTRIBUTING.md's new "Determinism seam gate" section names the
  ~150 production call sites still off-seam and the known-open gaps.
- **sim-testing:** a **determinism seam gate** now bans direct `Utc::now()`,
  `Instant::now()`, `SystemTime::now()`, and `Uuid::new_v4()` on the production
  code path of the modules that carry it (#1797). `clippy.toml` supplies the
  `disallowed-methods` config — each entry's `reason` names the replacement, so
  the error message teaches — and each gated module re-denies
  `clippy::disallowed_methods` via a `#![cfg_attr(not(test), deny(…))]` header,
  exactly mirroring the #1611 panic gate. Day-one manifest: `time.rs`,
  `entropy.rs`, `state.rs`, `scheduler.rs`, `app.rs`, `db.rs`, `job.rs`.
  `scripts/check-determinism-gate.sh` (new, self-testing, no toolchain, ~1s,
  wired into the CI `lint` job and `pre-push-check.sh`) guards the manifest,
  the header shape, per-site `reason` hygiene, the config's completeness, the
  absence of a module-wide `allow` spoof or a crate-local `clippy.toml` that
  would shadow the config entirely, and feature reachability. The manifest is a
  ratchet — it may grow, never shrink.
- **release:** every release with a breaking change now ships a migration guide,
  enforced by `scripts/check-migration-guides.sh` (#1588). Autumn ships every
  2–4 weeks and, pre-1.0, most releases can break existing apps, but the only
  automated check keyed off a `### Breaking` CHANGELOG heading this repo has
  never written — so it never fired, and 0.6.0 shipped the `with_pool` →
  `with_pool_untracked` rename with no guide entry at all. The new gate reads
  `CHANGELOG.md` and fails when a section declares a breaking change without a
  guide at `docs/migrations/<version>.md` (`next.md` for `## [Unreleased]`),
  when a breaking entry does not *link* its guide (a bare path mention is not a
  link), when an entry describes breaking something without the `**Breaking:**`
  marker the coverage check reads (explicitly non-breaking wording passes
  untouched), or when a guide is a stub — it must carry the `TEMPLATE.md`
  sections including *How to verify*, each with content under it, record the
  guide-only upgrade walk-through as `performed YYYY-MM-DD` rather than
  `pending`, and be indexed in `docs/migrations/README.md`. A release-candidate
  section (`## [0.7.0-rc.1]`) is gated against its release's guide. The gate
  fails closed on anything it cannot read — an unparseable `## ` heading and an
  unclosed code fence are hard errors, since either silently removes whole
  sections from every check. Fenced code blocks — backtick or tilde, of any
  fence length — are skipped wholesale in both the changelog and the guides
  (CommonMark closing rules included), so a config sample cannot
  turn a docs PR red and a guide cannot satisfy its own required headings from
  inside an example; the rolling `next.md` draft is exempt from the placeholder
  and empty-section checks, since the checklist recreates it from `TEMPLATE.md`
  after every release. It runs as its own `ci.yml` job on every pull
  request, so the guide is written by the author of the break while the change
  is still in review, and again in the publish gate for tags pushed outside a
  PR. Docs-only and free of any Rust build, it reports in seconds.
  `--list` prints the per-section inventory for the release operator.
  Guides are backfilled for `0.5.0` (new: the centralised
  `[security.trusted_proxies]` boundary and the rate-limit key deprecations)
  and `0.6.0` (the missing `with_pool` → `with_pool_untracked` section, scoped
  honestly — the published 0.5.0 crates have no pool constructor, so the rename
  is invisible to a crates.io upgrade and only bites `trunk-dev` trackers), and
  the `0.4.0`/`0.5.0` changelog sections gained the `### Breaking Changes`
  blocks they always deserved. `docs/release-checklist.md` gains a *Migration
  Guide Gate* section requiring the `next.md` rename, the changelog link
  repointing, and a **performed and recorded** guide-only upgrade of an
  `autumn new` app from the previous release — no changelog, no source reading —
  before `cargo publish`. `scripts/check-release-notes.sh` now shares the same
  marker convention instead of its own dead heading-only heuristic.
  The lint is textual and removes the *silent* failure mode rather than
  replacing review: a break described without the word "breaking" and without
  the marker still needs a reviewer to catch it. Entries that talk *about*
  breaking changes rather than being one carry an explicit, greppable
  `<!-- migration-guide-gate: reason -->` suppression — as this one does.
  [no-plugin] <!-- migration-guide-gate: describes the gate itself -->
- **generate scaffold / generate model:** a `lock_version` column now wires
  optimistic locking end to end, so two people editing the same scaffolded
  record can no longer silently clobber each other (#1318). Autumn already
  shipped the hard half — `#[lock_version]` plus the `RepositoryError::Conflict`
  the repository raises on a stale write (#575) — but the generator routed
  around it: the update handler hand-wrote an unconditional
  `diesel::update(table.find(id)).set(...)` and the edit form carried no
  version, so on a `lock_version`-bearing model the last write always won.

  Declaring the column (`autumn generate scaffold Post title:String
  lock_version:i32`) is now the whole opt-in. The model gets `#[lock_version]`
  and the migration `INTEGER NOT NULL DEFAULT 0` (the column is DB-managed, so
  the INSERT never names it). The edit form carries the row's current version
  in a hidden field — never as an editable control, and never on the *new*
  form. The `update` handler turns the write into a compare-and-swap,
  `WHERE lock_version = $expected` with `SET lock_version = lock_version + 1`
  in the same statement, so there is no read-modify-write window.

  A stale submit matches zero rows; the handler re-reads to distinguish "someone
  else got there first" (409) from "the row is gone" (404). The 409 re-renders
  the *same* edit form with the author's own input intact, an inline
  `role="alert"` banner, and the row's **current** version in the hidden field —
  so a second Save applies their edit on top of the newer row. Handing the stale
  version back would leave the form permanently unsavable. A `:states(...)`
  transition gets the same compare-and-swap: it is itself a read-modify-write
  (load, check the edge is legal from the state just read, write), so two
  concurrent transitions out of the same state would otherwise both commit. It
  guards on the version it read, 409s on a lost race, and bumps — so an author
  holding an older edit form also learns the record moved on.

  Coverage: the generated `tests/<snake>.rs` gains a
  `<plural>_optimistic_lock_conflict` test pinning the contract;
  `autumn-cli/tests/integration/scaffold_lock_version.rs` drives the real CLI
  and asserts the emitted model, migration, form and handler; and
  `generate_lock_version_postgres.rs` runs the generated migration and statement
  against real Postgres — including two concurrent transactions that both read
  the same version, where exactly one write lands.

  The retrofit path works too: `autumn generate migration AddLockVersionToPosts
  lock_version:i32` emits `ADD COLUMN ... NOT NULL DEFAULT 0`, which backfills
  existing rows in the same statement, and `autumn db pull` reproduces the
  attribute so a pulled table round-trips to the same model.

  Because the column name is load-bearing, `generate model`/`generate scaffold`
  now print a warning saying what declaring it changed and how to opt out
  (rename it). A `lock_version` that is not a non-nullable `i32`/`i64`, one
  marked `unique` (it is DB-managed and defaults to 0, so a unique index would
  reject the second row ever created), and and one that would leave a model with **no
  insertable columns at all** (every column database-managed means an empty
  `New{Model}`, whose Diesel `Insertable` derive does not compile) are all
  rejected at generation time rather than silently mis-generated — on the
  `generate model` path as well as `generate scaffold`, since the scaffold
  delegates its model planning there. `autumn db pull` declines the attribute
  in that same degenerate case (it mirrors a database it does not own, so it
  warns and pulls an ordinary integer rather than emitting a project that will
  not build). On HTML scaffolds,
  `--live`, `--sharded`, a `slug` column, and scaffolds with an `Attachment`
  column write through paths that do not route via the guarded statement, so
  combining them with `lock_version` is refused up front instead of emitting an
  edit form that only looks concurrency-safe (`--api` is exempt from those
  gates: it emits no form, so `--api --live` and friends keep generating). (`slug` in particular keys the update off an
  editable, reusable identifier, so `WHERE slug = ... AND lock_version = ...`
  would not pin a stable row.) The scaffolded **admin** update and the delete
  actions still bump-or-write without a guard and remain last-write-wins;
  locking across deletes is out of scope (#1021/#1312).

  A scaffold with no `lock_version` column is byte-identical to before, verified
  by diffing pre- and post-change generator output across twelve variants.

- **generate scaffold:** the generated list view ships a working **Export
  CSV** download (#1315). Autumn already had the hard half — `export_csv` +
  the `CsvSchema` trait, landed in #808 — but the generator never wired it,
  so every app author had to discover the trait, hand-write an impl, add a
  route, get the `Content-Disposition` quoting right and add a link, for
  every single model. `autumn generate scaffold` now emits all four: a
  `CsvSchema` impl covering `id`, every scaffolded column in declaration
  order and `created_at` (the `show` view's column set); a
  `#[get("/<plural>/export.csv")]` handler; an **Export CSV** link on the
  index; and a database-free generated test asserting 200, `text/csv`, an
  `attachment` disposition and the model's header row. `autumn-web`'s `csv`
  feature is enabled automatically (and removed again by `autumn destroy
  scaffold`).

  The export honours the **same** allowlisted `?sort=`/`?filter[col]=`
  params as the index, through the same `ListQuery` extractor and the same
  `repo.list` call (#1126) — and the index's link carries the current query
  string, so *filter → sort → export* downloads exactly the rows on screen.
  `?page=`/`?size=` are ignored: an export spans every page of the current
  filter. On a `--searchable` scaffold the link renders **inside** the
  `#<plural>-search-results` container rather than beside "New …", so the htmx
  swap that shows search results takes the link away with them: the search box
  pushes no URL and `ListQuery` has no full-text field, so a link left outside
  would survive the swap still pointing at the unsearched set and hand back the
  rows the user had just excluded. Searched results offer no export; clearing
  the search restores the list and its link. Rows are read in `MAX_PAGE_SIZE` batches and capped at
  `MAX_EXPORT_ROWS` (10 000), a constant in the generated file. An export that
  hits the cap is truncated rather than failed, but never silently: it logs a
  `warn!` and sets `x-export-truncated: true`. Distinguishing a complete export
  of exactly `MAX_EXPORT_ROWS` rows from a truncated one takes evidence a row
  exists past the cap, so the loop reads one batch beyond it and trims the
  surplus before writing the CSV. Consistency is per batch, not per export —
  offset batches take no shared snapshot, so a concurrent insert or delete can
  duplicate or skip a row — and the emitted docs say so rather than let the
  file imply a point-in-time read.

  Row-set posture mirrors the index exactly, so no new data path is opened: an
  owner-scoped scaffold's export is `#[secured]` and reads through the
  repository's owner-scoped `list_scoped`, never the unscoped `list`; a
  scaffold whose index carries no `#[secured]` gets an export that carries none
  either. COST does not mirror the index, so the handler additionally carries
  `#[throttle(limit = 6, per = "1m", key = "ip")]`: one export reads up to
  10 000 rows over ~100 page queries plus ~100 filtered `COUNT(*)`s — `list`
  counts before every page, and the export never reads the count but cannot
  opt out of it — where one index page costs two round trips. An inline
  throttle applies regardless of `security.rate_limit.enabled`. The emitted
  docs state that arithmetic so the cap, the throttle and the auth posture can
  be tuned against the real number rather than the row count.

  `NULL` columns serialize to an empty cell rather than the literal `None`, and
  commas/quotes/newlines are RFC 4180 quoted by `export_csv`. Text-backed
  columns additionally pass through an emitted `csv_text_cell` guard that
  prefixes an apostrophe to a value starting `=`, `+`, `-`, `@`, TAB or CR —
  RFC 4180 says nothing about formulas, and Excel executes them even inside
  quotes, so a row one user typed could otherwise exfiltrate the sheet when a
  colleague opens the download. Numeric, boolean, UUID, timestamp and enum
  columns are deliberately not guarded: they render from typed values and
  guarding them would corrupt a negative number.

  Emitted wherever the index's row set is a repository call the export can
  reuse verbatim — the plain `repo.list` index (including
  `--live-validation`) and the owner-scoped `list_scoped` one. Gated off for
  `--live`, `--sharded`, owner-scoped `--live-validation` and `--api`, whose
  output stays byte-identical. Two documented non-mirrors: a `--searchable`
  scaffold's export does not honour the search term (`ListQuery` carries no
  full-text query and the search box swaps results without changing the URL),
  and a `references` column exports the raw foreign key rather than the parent
  label the views resolve. See `docs/guide/generators.md`.

  `examples/bookmarks` drops its hand-rolled RFC 4180 quoting for the same
  `CsvSchema` + `export_csv` pair, keeping its range-capable `Download` demo.

- **system-tests:** `SystemTest::layer(...)` registers app-wide Tower
  middleware on the router a browser test serves (#1456). Applications whose
  routes depend on a global layer — tenant scoping bound to a database pool,
  an auth shim, request enrichment — previously had no way to install it on
  the runner, and the workaround (cloning the layer onto each handler before
  passing them to `.routes()`) exercises a stack the real app never serves.
  The method takes the same `IntoAppLayer` values as `AppBuilder::layer` (any
  `tower::Layer` whose service is `Infallible` — `from_fn` middleware and
  off-the-shelf tower-http layers alike) and routes them through the *same*
  router-assembly path, so the layer lands in the identical position — inside
  `RequestId` and the session layer, outside CSRF/CORS — and middleware that
  reads the request ID or session finds them under test exactly as in
  production. Multiple calls compose with `AppBuilder`'s ordering contract:
  the first registration is the outermost layer on ingress. Layers apply on
  both the default-state path and the `.state(...)` override path. See
  `docs/guide/system-tests.md`.

- **model:** declarative votable/reaction association via `#[votable(by =
  ..., aggregate = sum|count)]` (#1362). Votes, likes and favourites are the
  same shape every time — a `(reactor, target)`-unique edge table, a
  toggle/flip/insert on it, and a denormalised `score` / `{name}_count` on the
  target that must stay exactly equal to `SUM(value)` / `COUNT(*)` — and
  hand-writing it is a read-then-write race on the edge *plus* a lost-update
  race on the aggregate whenever two *different* reactors touch one target.
  Declaring `#[votable(by = User, aggregate = sum)]` on a `#[model]` now emits
  the edge table's `diesel::table!` into a hidden per-association module (the
  `through =` many-to-many pattern from #1324, so it never collides with a
  hand-written `schema.rs` entry) plus a `{Model}Reactions` trait
  blanket-implemented for that model's `#[repository]` — no repository
  attribute, no macro changes on the repository side. The trait gives
  `react(reactor_id, target_id, value) -> Reaction { value: Option<i16>,
  aggregate: i64, outcome: Inserted|Flipped|Removed }` and
  `reaction_of(reactor_id, target_id) -> Option<i16>`
  (`autumn_web::repository::{Reaction, ReactionOutcome}`); `aggregate = count`
  emits `react(reactor_id, target_id)` with no `value` parameter, since a
  unary-like edge row is pure membership. `react()` is a race-safe toggle
  (not idempotent — a blind retry of a timed-out call inverts the outcome, so
  retry safety belongs in an HTTP-layer idempotency key): the same value again
  toggles the edge off, a different value flips it in place, a new one inserts
  it, and the aggregate is recomputed from ground truth (`SUM`/`COUNT`, never
  accumulated as a delta, so pre-existing drift self-heals) and persisted
  **in the same transaction**, under a row lock on the target held across the
  whole read-decide-write-recompute window (`SELECT ... FOR NO KEY UPDATE` on
  Postgres — the weaker mode, so concurrent referencing inserts such as a new
  comment on the same post are not blocked; `BEGIN IMMEDIATE`'s write lock on
  SQLite). N concurrent reactions on one
  target therefore converge to at most one edge per `(reactor, target)` with
  no `23505` escaping to any caller, the persisted aggregate is exact even
  across different reactors, and a reader never observes edge/aggregate
  disagreement — verified against real Postgres by 50 simultaneous clicks on
  one pair. Every name is inferred with an override for each (`name`, `table`,
  `reactor_fk`, `target_fk`, `value_column`, `column`); the defaults resolve
  to `votes` / `user_id` / `post_id` / `value` / `score`, which is why
  `examples/reddit-clone` adopted it with **no migration and no overrides**,
  collapsing ~90 lines of hand-written toggle/flip/upsert SQL and a raw
  `sql_query` score recompute in `src/routes/votes.rs` to a single
  `posts.react(...)` call, leaving zero raw SQL in that file. The
  composite `UNIQUE (reactor_fk, target_fk)` on the edge table is load-bearing
  (it is the `ON CONFLICT` arbiter) and remains the app's migration to write,
  as is a `CHECK` on the value column: `react()` does **not** validate `value`,
  so never bind it straight from a request. A nullable target FK in the DDL is
  tolerated (reddit-clone's `votes` is an XOR over `post_id`/`comment_id`) —
  the unique constraint then covers only the non-`NULL` rows, which are exactly
  the ones this association writes. The model's `#[id]` and aggregate field
  must both be `i64`, checked at compile time, and `#[votable]` must be written
  **below** `#[model]`. Soft-delete aware: when
  the target model has a `deleted_at` field, reacting to a soft-deleted target
  is `NotFound` and leaves its aggregate untouched, matching the repository
  layer's scoping. The view half is the new no-JS
  `autumn_web::widgets::{ReactionControls, reaction_controls}` (also prelude
  re-exported): one `<form method="post">` per direction carrying the hidden
  CSRF input when a token is threaded,
  upgraded in place by htmx (`hx-swap="outerHTML"` onto the control's own
  `dom_id`), ARIA toggle buttons with real accessible names and
  `aria-pressed`, and an `aria-live` aggregate. The hidden CSRF input is what
  makes the JavaScript-off path work — thread `.csrf(...)` on any page a no-JS
  visitor can reach, or the plain form POST is rejected while the htmx path
  keeps working via the header shim. Known limits, all documented:
  at most one `#[votable]` per model (a second is a directed compile error),
  the recompute is O(edges per target), writes to one target serialise, READ
  COMMITTED is assumed (a contended locking read blocks first and then fails
  with `40001` rather than corrupting), `react()` bypasses model hooks and does
  not touch `updated_at`, every edge *write* must go through `react()` for the aggregate to
  stay exact, and — like the m2m mutation helpers — `react()` acquires its
  **own** pooled connection and does not join an enclosing `Db::tx`, so a
  handler must not hold a `Db` extractor across the call (that needs two
  connections at once and deadlocks once concurrency reaches the pool size).
  `reaction_of()` is a read and routes through the repository's read route, so
  it is replica-eligible and does not pin read-your-writes — re-render from the
  `Reaction` the write returned.
  Tenant isolation is **enforced**, not left to the caller: when the target
  model has a `tenant_id` column and the repository is
  `#[repository(..., tenant_scoped)]`, the target lock (S1) and the aggregate
  `UPDATE` (S5) both carry `tenant_id = <current tenant>`, so a caller who
  guesses another tenant's `target_id` gets `NotFound` before any edge insert or
  aggregate write, and `reaction_of()` returns `None` for a foreign-tenant
  target instead of that tenant's reaction. A `tenant_scoped` repository with no
  tenant context fails closed with the same "no tenant context was established"
  error its derived finders raise, and `across_tenants()` opts out of the
  predicate exactly as it does for a finder — resolved through a new hidden
  `M2mConnSource::__autumn_m2m_tenant_scope()` so the repository's own scoping
  decision (not field presence) is what applies. A model without a `tenant_id`
  column emits no tenant branch at all and is unchanged. The many-to-many
  `add_*`/`remove_*`/`set_*` helpers keep the old id-only scoping (pre-existing,
  tracked separately).
  Contrary to the issue's parenthetical suggestion, the recompute does **not**
  reuse `repository_commit_hooks`: that is a durable post-commit queue running
  on a different connection, which structurally cannot be atomic with the edge
  mutation — see `docs/adr/0008-associations-and-eager-loading.md` for the
  reasoning and the rejected lock-free/CTE/delta designs. Purely additive; no
  existing association, model, or repository behaviour changes; minor version
  bump. See the new `docs/guide/votable.md`.
- **metrics:** new **call-site metrics facade** (`autumn_web::metrics`, #1378)
  so application code can record its own counters, gauges, histograms and
  timers in one line at the point the interesting thing happens — no trait to
  implement, no type to define, nothing to register with `AppBuilder`:
  `metrics::counter("checkout_completed_total").with_label("status", "paid").increment(1)`
  registers the instrument in a process-global registry on first use and shows
  up on the stock `/actuator/prometheus` scrape (and under a new top-level
  `app` key on `/actuator/metrics`) with no wiring at all. `timer(..).start()`
  returns a guard that records on every exit path including early `?` returns
  and unwinding panics, plus `time()`/`time_async()`/`record()` for cases a
  guard does not fit; histograms render as standard cumulative Prometheus
  histograms whose `le="+Inf"` bucket is structurally derived from the same
  slots as `_count`, so the two can never drift. The facade is designed to
  degrade rather than take an app down: hard caps (100 labeled series per
  instrument, 256 instruments, 8 labels, 128-char label values, 128-byte
  names) drop or reject the excess rather than grow without bound, with one
  warning per instrument and a scrapeable
  `autumn_metrics_series_dropped_total{metric="..."}` counter so cardinality
  mistakes are alertable; invalid, `autumn_`-prefixed, kind-conflicting and
  derived-name-colliding registrations are rejected with a warning and an inert
  handle rather than a panic or a malformed scrape. Recording is never gated —
  `actuator.prometheus = false` removes only the *scrape endpoint*, and
  `/actuator/metrics` still carries the `app` key (the router now logs that at
  startup, naming the config key). Startup configuration is order-free:
  `describe_*` does **not** register the instrument, it stashes the help text
  until the first use does, so `describe_histogram` can no longer freeze a
  histogram's bounds out from under a later `set_histogram_buckets` (the cost
  is that a described-but-never-recorded metric stays out of the scrape
  entirely). Gauges and histograms accept `usize`/`u64`/`i64` as well as the
  `Into<f64>` primitives through a sealed `IntoMetricValue`, so
  `gauge("q").set(queue.len())` compiles without a cast; counters saturate at
  `u64::MAX` rather than wrapping (a wrapped total is indistinguishable from a
  reset and would hand `rate()` a phantom spike); and non-finite values are
  rejected on gauges as well as histograms, so the Prometheus and JSON views
  can never disagree. The exposition output is hardened to match Prometheus'
  own parser and `client_golang`: bucket `le` values render byte-for-byte as Go
  `%g` does (`5e-05`, `1e+21`), the built-in request-duration summary reserves
  its derived `_sum`/`_count` names against plugin families, label values and
  `# HELP` text are stripped of control characters, over-long names are
  rejected instead of truncated into a collision, rejected names are escaped
  before they reach a log line, and which labels survive the 8-label cap is a
  function of the label *set* rather than the order `with_label` was called in.
  New guide: `docs/guide/metrics.md`, including a facade-vs-`MetricsSource`
  comparison.
- **cli,web:** `autumn generate scaffold` now ships a **no-JavaScript
  bulk-select + delete-selected flow** on every standard HTML index list
  (#1312). The `data_table` gains a leading
  `bulk_select_checkbox` column, the list (and, with `--searchable`, the whole
  htmx-swapped results container) is wrapped in a `bulk_actions_form` posting
  to a new `#[secured] POST /{plural}/bulk_delete` handler, and `src/main.rs`
  mounts that route immediately after `destroy`. The handler parses the
  repeated checkboxes with a generated `parse_bulk_ids` helper (deduping through
  a `HashSet`, so a crafted body carrying many distinct `ids` parses in linear
  rather than quadratic time, and stopping one past a `MAX_BULK_IDS` cap of
  5000 — a real selection is page-sized, but the default 32 MiB request limit
  otherwise leaves room for over a million ids; an oversized batch is refused
  with an error flash rather than truncated, since a silently partial
  destructive batch is worse than a refused one), chunks its pre-flight
  `SELECT` at 1000 ids (`eq_any` binds one parameter per id and
  `MAX_BIND_PARAMS` is 32766 on SQLite, so one unbounded `eq_any` would fail
  with "too many SQL variables" before reaching the already-chunked
  `delete_many`), per-row
  authorizes with the same `"delete"` action `destroy` uses when policy wiring
  is on (an unauthorized row is dropped from the batch, not 403'd, so the
  endpoint is no existence oracle), routes the delete through
  `repo.delete_many` so soft-delete/hooks/`dependent(...)` cascades all apply,
  flashes the deleted count, and 303s back to the index. The handler `drop`s
  its `Db` extractor between the pre-flight `SELECT` and `delete_many` — `Db`
  holds its connection until dropped and `delete_many` checks out one of its
  own, so keeping both would stall every bulk delete on
  `database.pool.max_size = 1` and deadlock a larger pool under enough
  concurrency. An empty or malformed
  selection redirects without error — a list-write endpoint never 400s on bad
  params. The checkbox field is deliberately `name="ids"` (not `ids[]`),
  matching `autumn-admin-plugin`'s existing bulk-action contract; the parser
  accepts the `ids[]` spelling too, for clients that send it. Three reusable
  widgets back it for hand-written views: `autumn_web::widgets::{
  bulk_select_checkbox, bulk_actions_toolbar, bulk_actions_form}` plus a
  `BulkActionsConfig` builder. The toolbar deliberately emits no confirmation
  prompt: an inline `onclick="return confirm(..)"` is blocked by Autumn's
  default `script-src 'self'` CSP (the form would submit with no prompt, worse
  than not promising one), and `confirm_action` — the framework's
  server-rendered `window.confirm()` replacement — posts its own single-action
  form, so it cannot carry a bulk form's checkbox selection. Confirm a batch
  with an interstitial page instead. The bulk form carries a one-time `_submit_token`
  (#1360) ahead of the checkboxes, so a double-click or Back→resubmit replays
  the first response instead of re-running the batch: `SubmitTokenLayer` passes
  a tokenless request straight through, and it only scans the body's first
  chunk, so a long selection must not be able to push the field past the scan
  cap. Gated off for
  `--live`/`--live-validation`/`--sharded`/`--api`, whose output stays
  byte-identical.
- **sim-testing:** fix a genuine **job-backoff thundering herd** the
  deterministic simulation harness caught (W7, #1797): the local job
  runtime's retry backoff (`execute_local_job`, `job.rs`) computed a pure
  exponential delay (`initial_backoff_ms * 2^(attempt-1)`) with no jitter —
  a function of a job's *configuration* only, not its identity — so when
  several jobs in the same queue fail at the same instant (a downstream
  dependency blip), every one of them retried at the *exact same* instant,
  immediately re-flooding the dependency it just backed off from. The new
  `jittered_retry_delay_ms` draws an equal-jitter spread
  (`[base.div_ceil(2), base]`, never *longer* than the un-jittered delay) from
  the framework's injected `Entropy` seam (`state.entropy()`) — real OS
  entropy in production, seeded and bit-for-bit reproducible under a
  `#[sim_test]` seed; the ceiling (not plain integer division) keeps a small
  configured `backoff_ms` (e.g. `1`) from rounding its floor down to an
  immediate 0ms retry (Codex review). A real-clock
  integration test cannot deliberately reproduce this bug (it would need N
  real jobs to fail within the same millisecond); the new worked-example test
  (`tests/integration/sim_retry_storm.rs`) builds the adversarial condition
  directly by enqueuing N jobs that all fail their first attempt on the
  sim's paused runtime, then asserts via `always!`/`sometimes!` that their
  retries spread across more than one checkpoint of the backoff window — the
  exact class of whole-app concurrency bug DST (#1797) exists to catch.
  Verified the test actually catches the regression: temporarily reverting
  `jittered_retry_delay_ms` to the old un-jittered formula reproduces the
  herd (all 12 retries land in the final checkpoint bucket) and fails the
  `always!`; restoring the fix passes it again. Entropy injection is opt-in
  under the sim (`Sim::build` wires the virtual clock automatically but not
  entropy), and the test's first draft missed wiring it, so its jitter
  silently drew from real OS randomness rather than `AUTUMN_SIM_SEED` (Codex
  review) — fixed by mounting with `.with_entropy(SeededEntropy::new(sim.seed))`,
  proved by a new `retry_checkpoints_replay_deterministically_from_the_seed`
  test that runs the same seed twice and asserts on the identical outcome.
  New guide
  `docs/guide/simulation-testing.md` walks through `#[sim_test]`, virtual
  time, deterministic entropy, chaos, `always!`/`sometimes!`, the seed-sweep
  runner, and this worked example end-to-end. [no-plugin]
- **pdf:** render server-side templates to downloadable PDF documents
  (#1317). A new opt-in `pdf` Cargo feature adds `autumn_web::pdf::Pdf`, an
  `IntoResponse` built on the existing `Download` responder — `Pdf::from_html`
  or (with the `maud` feature) `Pdf::from_markup` renders an HTML string into
  a PDF with `Content-Type: application/pdf` and an RFC 6266-safe
  `Content-Disposition` (`.filename(...)`, `.inline()`). Rendering uses a
  deliberately small HTML subset (headings, paragraphs, tables, lists,
  bold/italic, `br`/`hr`) with the PDF base-14 fonts — no system-installed
  browser and no embedded font files, keeping PDF generation compatible with
  the single-binary story (#1004); any other tag passes its text through
  transparently instead of being dropped. Rendering the same input always
  produces the same visible text (no wall-clock or other hidden state is
  read), verified via a new `TestResponse::assert_pdf_contains` test helper
  backed by `autumn_web::pdf::extract_text`. A new `examples/invoice` app
  demonstrates rendering the same Maud view for both an on-screen detail page
  and its PDF export, and the "Generating PDFs" guide
  (`docs/guide/pdf-downloads.md`) covers the supported HTML subset and the
  determinism/testing story.
- **release:** `autumn release init --target azure-container-apps` (#1278)
  scaffolds a production-ready Azure deployment alongside the existing `fly`
  and `docker-compose` targets: `main.tf` (resource group, Azure Container
  Registry, Log Analytics workspace, Container Apps environment + the app
  itself, a one-shot migration job, Azure Database for PostgreSQL Flexible
  Server, and a Key Vault that feeds
  `AUTUMN_DATABASE__PRIMARY_URL`/`AUTUMN_SECURITY__SIGNING_SECRET` into the
  app as secret refs via a user-assigned managed identity (with its own Key
  Vault access policy granted to Terraform's caller identity, since
  access-policy-model vaults grant no data-plane access by default), and an
  optional Redis Cache gated behind `enable_redis_cache`, wired in as
  `AUTUMN_CACHE__BACKEND=redis` / `AUTUMN_CACHE__REDIS__URL` (Autumn's
  actual config path) with its access key `urlencode()`'d before being
  written into Key Vault — infrastructure only, though: the app must
  separately depend on `autumn-cache-redis` and register `RedisCachePlugin`
  in `main.rs` for these env vars to take effect, which the generated
  `main.tf`/`variables.tf` now say explicitly), `variables.tf`
  (`app_name`, `location`, `image_tag`, `db_sku`, `bootstrap_image`,
  `min_replicas`/`max_replicas` — defaulting to the 1/10 scale range — and
  `enable_redis_cache`, plus `sensitive`, no-default secret variables for
  `database_admin_password`/`signing_secret`),
  `outputs.tf` (`app_fqdn`, `acr_login_server`, `resource_group_name`,
  `migrate_job_name`, `app_name`), a `terraform.tfvars.example` that
  documents non-secret defaults without ever committing a literal secret,
  and `.github/workflows/azure-deploy.yml` — an opt-in OIDC-based workflow
  (triggers only on a `v*` tag push or manual dispatch, and every credential
  comes from GitHub secrets/variables that don't exist until configured — no
  client secret needed) that builds the release image, pushes it to ACR,
  runs the migration job to completion (aborting the deploy if it fails),
  and runs `az containerapp update`. The database connection string is
  derived inside Terraform from the Postgres server the same apply creates
  rather than taken as an input variable, so a single `terraform apply` is
  enough. Every Container Apps-family resource name (the app, its
  environment, Log Analytics, the migration job) is sanitized to lowercase
  alphanumerics-and-hyphens, with hyphen runs collapsed, a leading/trailing
  hyphen trimmed, a too-short result padded to Azure's 2-character minimum,
  and the base capped at 24 characters (headroom for the longest suffix,
  `-migrate`, so the full name never exceeds Azure's 32-character maximum)
  — a Cargo package name may legally be as short as one character, longer
  than 32, contain underscores/uppercase, or produce adjacent hyphens when
  mapped, all invalid or malformed there — computed once in Terraform
  (`local.app_name_safe`) and exposed as the `app_name` output; the
  generated workflow reads it back as a variable rather than ever hardcoding
  a name, so editing `app_name` in `terraform.tfvars` after scaffolding is
  picked up automatically. The workflow also maps every character outside
  Docker's `[A-Za-z0-9_.-]` tag charset in `GITHUB_REF_NAME` to `-` and caps
  it at 128 characters before using it as an image tag (a `workflow_dispatch`
  branch may contain `/`, and a SemVer tag like `v1.2.3+build` contains `+`,
  both invalid in a Docker tag) and documents that its service principal
  needs Contributor at the
  resource-group scope, not just on the Container App — RBAC on the app
  doesn't inherit to the sibling migration job. The app and migration job
  both start from a public placeholder image (`bootstrap_image`) since
  Container Apps must pull an image to create a first revision and a
  brand-new ACR has none yet; Terraform ignores further image drift once CI
  takes over.
  `autumn release init`'s file-existence guard and directory creation are
  now generic over nested output paths, so `--force`/collision checks cover
  `.github/workflows/azure-deploy.yml` the same way they cover root-level
  files. It also merges Terraform's `.gitignore` entries
  (`.terraform/`/`*.tfstate*`/`terraform.tfvars`) into the project's
  `.gitignore` — idempotently, preserving unrelated lines — since Terraform
  state holds every secret value in plaintext regardless of a variable's
  `sensitive` flag. `main.tf` also sets a new required `subscription_id`
  variable on the provider (AzureRM v4 made it mandatory even under `az
  login` CLI auth), bounds `local.app_name_alnum` (ACR/Postgres/Redis) to 30
  characters so a long Cargo package name can't overflow ACR's 50-character
  limit, bounds the Postgres database name to 63 characters for the same
  reason, and no longer pins the Postgres Flexible Server to availability
  zone 1 (not every Container Apps region offers it, and a hardcoded zone
  fails `terraform apply` in those regions even though an unzoned server
  would succeed). `terraform.tfvars.example` generates
  `database_admin_password` as `openssl rand -base64` plus a fixed
  upper/lower/digit/symbol suffix — Azure's Postgres complexity policy
  needs 3 of those 4 categories, `-hex` output is lowercase-only, and even
  `-base64` alone only samples its alphabet randomly rather than
  guaranteeing coverage. The generated workflow's image tag now always
  includes the commit SHA (not just the sanitized ref) so two
  `workflow_dispatch` runs on the same branch can never collide on an
  identical tag — re-pushing bytes under a tag the Container App already
  has configured isn't guaranteed to register as a revision-scope change —
  and the job sets a per-repository `concurrency` group with
  `cancel-in-progress: false` so overlapping runs queue instead of racing
  each other's migration/cutover ordering, plus an explicit staleness guard
  immediately before migrating — compared against GitHub's own `run_number`
  (monotonic in trigger order regardless of which ref triggered a run,
  since two *different* immutable tags each trigger their own run against
  their own never-moving ref, so a same-ref check alone can't see a newer
  release land under a different tag) rather than merely whether the
  triggering ref itself has moved, and aborts if a run with a higher
  `run_number` is still queued/in progress OR has *already completed
  successfully* — otherwise a run GitHub scheduled after this one but that
  finishes deploying first would go undetected, and this older run would
  migrate/deploy right over it. The computed image tag can
  no longer start with an invalid character either (Docker's tag grammar
  requires a leading word character; sanitizing e.g. a `+hotfix` ref could
  otherwise leave a leading `-`). `main.tf`
  also derives the Container App's default-ingress hostname from its
  environment (`azurerm_container_app_environment.this.default_domain`,
  computable before the app itself exists — referencing the app's own
  only-known-after-create FQDN would be circular) and sets it as
  `AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS`: `AUTUMN_PROFILE=prod` makes
  Autumn's `fail_fast_on_invalid_trusted_hosts` exit immediately when that
  list is empty, so without this the container would never bind after the
  first real deploy. The same derived hostname (not
  `latest_revision_fqdn`, which names a specific revision rather than the
  stable ingress endpoint, and would go stale the moment CI creates a new
  revision outside Terraform) is exposed as the `app_fqdn` output. The
  manual deploy walkthrough in `docs/guide/deployment.md` now actually
  blocks on migration success too — `az containerapp job start` only starts
  an execution and returns immediately, so a bare shell comment telling the
  reader to "wait for Succeeded" let the following `az containerapp update`
  run before migrations had actually finished; it's now a real polling loop
  mirroring the generated workflow's. The `.gitignore` merge
  (`ensure_azure_gitignore_entries`) now applies git's own "last matching
  rule wins" semantics instead of a naive exact-line presence check: an
  existing `.gitignore` with `terraform.tfvars` followed later by any
  negation — an exact `!terraform.tfvars`, or a broader wildcard like
  `!*.tfvars`/`!*` — actually leaves the file trackable despite the
  earlier line being present, so any of those cases now re-append the
  entry after the negation rather than wrongly treating it as already
  protected (matching gitignore's full glob syntax is out of scope, so any
  negation line is conservatively treated as potentially applying, at
  worst causing a harmless extra re-append rather than a false sense of
  protection). Reading an existing `.gitignore` that fails for a reason
  other than not existing (invalid UTF-8, a permission error) now
  propagates that error instead of silently treating it as empty and
  overwriting the file's real content with just the Terraform entries —
  and does so BEFORE any scaffold file is written, so that failure never
  leaves a partial, complete-looking scaffold on disk that then blocks a
  retry without `--force` on files this same call just created. The
  migration poll budget in both the generated workflow and the manual
  walkthrough is now 660s, comfortably past the migration job's own
  `replica_timeout_in_seconds` (600s) — polling any shorter risked
  reporting "timed out" on a migration that was still validly running (and
  would have succeeded) while leaving it to keep mutating the schema in
  the background after the deploy had already been abandoned. `docker
  build` (both the generated workflow and the manual walkthrough) now
  passes the `AUTUMN_BUILD_*` `--build-arg`s the Dockerfile declares —
  without them every Azure-deployed image reported null git provenance at
  `/actuator/info`, since those ARGs default to empty and `.dockerignore`
  excludes `.git` from the build context. The resource group and
  user-assigned identity now use the same length-bounded
  `local.app_name_safe` as every other Container Apps-family resource
  instead of raw `var.app_name`, which — unlike the character-set
  sanitization those two already got — was still unbounded in length: a
  Cargo package name longer than 87 characters overflowed the resource
  group's own 90-character limit once `-rg` was appended. Both base
  sanitization locals (`app_name_alnum`, `app_name_hyphenated`) now fall
  back to a fixed `app` prefix whenever the input sanitizes to nothing or
  to a value not starting with a letter — a legal-but-unusual Cargo
  package name like `_` previously sanitized to an empty string, producing
  a Postgres server name starting with `-`, an empty Postgres database
  name, and violating resource types (Key Vault) that require a
  letter-led name rather than just an alphanumeric one. The manual deploy
  walkthrough's image tag is no longer a fixed `v1` either — like the
  generated workflow, it now derives a unique tag so a second manual
  deploy can't reuse a tag the app already has configured and risk not
  registering as a new revision; the commit SHA alone still wasn't enough
  (re-running the walkthrough at the same `HEAD` — uncommitted local
  changes, or merely a fresh `AUTUMN_BUILD_TIMESTAMP` — pushes different
  bytes under the same tag), so it now also folds in a UTC timestamp,
  unique per build rather than per commit. The generated
  workflow's own image tag now also folds in `GITHUB_RUN_ID`/
  `GITHUB_RUN_ATTEMPT`, not just the ref and commit SHA — re-running
  `workflow_dispatch`, or clicking "Re-run jobs" on an existing run, reuses
  the identical ref and commit while still producing a genuinely different
  build (a fresh `AUTUMN_BUILD_TIMESTAMP`, possibly different base-image
  bytes), so a tag built only from ref+SHA could still collide with a
  previous run's. The run-ordering staleness guard no longer filters by
  status or conclusion either — it now rejects a run the moment ANY other
  run of the workflow with a higher `run_number` exists, regardless of
  outcome: a newer run can migrate (the actual point of no return) and
  then fail on a later step, reporting an overall conclusion of `failure`
  that a status-filtered check would have missed entirely. The Postgres
  Flexible Server's `administrator_login` is no longer `autumn_admin` —
  Azure only allows alphanumeric characters there, so the underscore made
  `terraform apply` fail while creating the server — it's now a single
  `local.postgres_admin_login` shared by the server resource and the
  derived `database_url` secret, so the two can't drift out of sync. The
  Postgres database name now also guards against Azure Flexible Server's
  reserved system database names (`postgres`, `azure_maintenance`,
  `azure_sys`) — a Cargo package literally named one of those (or e.g.
  `azure-sys`, which sanitizes to the same underscored name) previously
  derived an identical database name, and `terraform apply` failed trying
  to create/manage a database a fresh server already owns; it now appends
  a `_prod` suffix in that case, still within the 63-byte limit. The
  generated Azure Redis Cache disables its non-TLS port, so the app only
  ever receives a `rediss://` URL, but the workspace `redis` dependency was
  built with no TLS Cargo feature — `redis::Client::open` rejected that
  scheme outright ("can't connect with TLS, the feature is not enabled"),
  so the optional cache path was entirely unusable the moment
  `enable_redis_cache` was turned on. The workspace `redis` dependency now
  also enables `tokio-rustls-comp`, matching the rustls stack the rest of
  the workspace already standardizes on. Both the generated workflow and
  the manual deploy walkthrough now update the migration job's image via
  `az containerapp job update --image ...` before starting it, instead of
  passing `--image` straight to `az containerapp job start`: the latter
  sends an execution-template *override*, which Azure treats as a full
  replacement rather than a merge, silently dropping the Terraform-
  configured `command` (`autumn migrate`) and the
  `AUTUMN_DATABASE__PRIMARY_URL` secret env — the execution would run the
  container's default command with no DB URL instead of applying
  migrations. `autumn release init --target azure-container-apps` also now
  warns when run from a Cargo workspace member directory whose git
  repository root differs from the current directory: GitHub Actions only
  discovers workflow files under the repository ROOT's
  `.github/workflows/`, so the generated `azure-deploy.yml` would
  otherwise sit somewhere it can never fire from, with no indication why
  tag pushes and manual dispatches never trigger it. The warning names the
  actual git root and the exact `working-directory:` override needed if
  the file is moved there by hand. The `AcrPull` role assignment on the
  freshly-created user-assigned identity now sets
  `skip_service_principal_aad_check = true`: without it, Entra ID
  replication lag can make `terraform apply` intermittently fail with
  `PrincipalNotFound` on a fresh apply, even though the identity it's
  granting the role to was just created successfully by that same apply.
  The Postgres reserved-database-name guard now also covers `template0`
  and `template1` — every Postgres cluster, on any host, is initialized
  with those two as its own templates, not just the three Azure-specific
  system databases the guard already listed; an app literally named either
  one previously collided the same way. The generated `.dockerignore` now
  also excludes `.terraform/`/`*.tfstate*`/`terraform.tfvars`: without
  those, running `docker build .` from the crate directory (where
  `autumn release init --target azure-container-apps` scaffolds Terraform
  files) after `terraform apply` uploaded the plaintext state file — every
  secret value, `sensitive` flag or not — into the build context/cache
  even though no stage ever copies it into the final image.
- **release:** `autumn release init --target aws-app-runner` and `--target
  aws-ecs` (#1279) — two AWS deployment targets, meeting teams where they
  are the same way `fly`/`azure-container-apps` do. `aws-app-runner` is the
  fast/minimal path: an ECR repository, a VPC (private subnets for RDS + an
  App Runner VPC connector, plus a NAT gateway so the app's own outbound
  traffic keeps working once App Runner routes ALL egress through the
  connector), RDS PostgreSQL, Secrets Manager entries for the database URL
  and signing secret, and the App Runner service itself — no CI workflow,
  wire up your own once you outgrow it. `aws-ecs` is the production path: a
  VPC with public/private subnets across 2 AZs, an internet-facing ALB
  (HTTP→HTTPS redirect, an ACM certificate via Route 53 DNS validation), an
  ECR repository, an ECS Fargate cluster/task definition/service (deployment
  circuit breaker with automatic rollback), Application Auto Scaling on
  CPU/memory (desired 2, min 1, max 10), RDS PostgreSQL, Secrets Manager, a
  one-shot migration task definition, an optional ElastiCache Redis
  replication group gated behind `enable_redis_cache` (same
  infrastructure-only caveat as Azure's Redis Cache — the app must also
  depend on `autumn-cache-redis` and register `RedisCachePlugin`), and
  `.github/workflows/aws-deploy.yml` — an opt-in OIDC-based workflow
  (`AWS_ROLE_ARN`, no long-lived access keys) that builds the release image,
  pushes it to ECR, registers new "app" and "migrate" task definition
  revisions (task definitions are immutable per revision — this describes
  the current one, swaps in the new image via `jq`, and re-registers it,
  leaving every other Terraform-declared setting untouched), runs the
  migration task to completion via `run-task` + a poll loop (aborting the
  deploy if it fails or the container exits non-zero), then updates the ECS
  service and waits for `services-stable`. Both targets derive the database
  connection string inside Terraform from the RDS instance the same apply
  creates rather than taking it as an input variable, so a single `terraform
  apply` is enough. Both start their bootstrap resources (the App Runner
  service; the "app" and "migrate" ECS task definitions) from a public
  placeholder image — App Runner/Fargate must pull *some* image to create a
  first revision, and a brand-new ECR repository has none yet — and
  `lifecycle.ignore_changes` stops a later `terraform apply` from reverting
  a live deploy back to it. AWS resource names (ECR, RDS identifiers, the
  ALB/target group — the tightest, at a 32-character AWS limit — App
  Runner, the VPC connector) are sanitized the same way as Azure's Container
  Apps-family names: lowercased, mapped to alphanumerics-and-hyphens,
  hyphen runs collapsed, a leading/trailing hyphen trimmed, capped at 20
  characters for headroom, with a fixed "app" fallback when that leaves
  nothing or a non-letter-leading value (RDS identifiers must start with a
  letter). Unlike App Runner's own assigned subdomain (only known after the
  service is created, so `AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS` is set on
  the same follow-up CLI call that deploys the real image, not the first
  `terraform apply`), ECS's ALB serves under an operator-supplied
  `domain_name`/`route53_zone_id` pair, known before the apply even starts,
  so trusted hosts are set correctly from the first apply. IAM is scoped to
  least privilege throughout: the App Runner instance role and the ECS
  execution role can each only `secretsmanager:GetSecretValue` the specific
  secret ARNs this app uses, never a wildcard, and ECS's execution role
  (image pull/logs/secrets injection) and task role (the running
  container's own AWS permissions) are separate principals. `autumn release
  init`'s Terraform `.gitignore` merge (`.terraform/`/`*.tfstate*`/
  `terraform.tfvars`) and its nested-workflow-relocation warning (GitHub
  Actions only discovers `.github/workflows/` at the git repository root,
  not a Cargo workspace member subdirectory) — both previously
  Azure-specific — now cover all three Terraform targets generically.
- **release:** `autumn release init --target gcp-cloud-run` (#1280) — a GCP
  deployment target alongside `fly`/`azure-container-apps`/`aws-app-runner`/
  `aws-ecs`. `main.tf` provisions an Artifact Registry repository, a VPC with
  a Serverless VPC Access connector, Cloud SQL for PostgreSQL on a private IP
  (no public exposure), a dedicated runtime service account scoped to
  `roles/cloudsql.client` plus per-secret `secretAccessor` grants (never a
  project-wide `secretAccessor` binding), Secret Manager entries for the
  database URL and signing secret, the Cloud Run service itself (min 1 / max
  10 instances), a one-shot Cloud Run Job that runs `autumn migrate`, and an
  optional Memorystore Redis instance gated behind `enable_redis_cache` (same
  infrastructure-only caveat as Azure's Redis Cache and AWS's ElastiCache —
  the app must also depend on `autumn-cache-redis` and register
  `RedisCachePlugin`). `.github/workflows/gcp-deploy.yml` is an opt-in
  Workload-Identity-Federation-based workflow (no service account key) that
  builds the release image, pushes it to Artifact Registry, updates and
  executes the migration job to completion via `gcloud run jobs execute
  --wait` (a native synchronous wait — no manual poll loop needed, unlike the
  Azure/AWS targets' workflows), then updates the Cloud Run service. The
  database connection string is derived inside Terraform from the Cloud SQL
  instance the same apply creates rather than taken as an input variable, so
  a single `terraform apply` is enough. Both the service and the migration
  job start from Google's public Cloud Run "hello" quickstart image
  (`bootstrap_image`) — Cloud Run must have *some* image to create a first
  revision, and a brand-new Artifact Registry repository has none yet — and
  `lifecycle.ignore_changes` stops a later `terraform apply` from reverting a
  live deploy back to it. GCP resource names are sanitized the same way as
  the other Terraform targets (lowercased, mapped to alphanumerics-and-
  hyphens, hyphen runs collapsed, a leading/trailing hyphen trimmed, a fixed
  "app" fallback when that leaves nothing or a non-letter-leading value).
  Unlike App Runner's own assigned subdomain (only known after the service is
  created), Cloud Run's default URL format
  (`<service>-<project number>.<region>.run.app`, using the project
  *number*, not the project ID) is derivable at plan time from a
  `google_project` data source, so `AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS` is
  set correctly from the very first `terraform apply` — no second apply or
  post-create CLI call needed. `autumn release init`'s shared Terraform
  `.gitignore` merge and nested-workflow-relocation warning now cover all
  four Terraform targets.
- **i18n:** locale-prefixed routing and a path-preserving locale switcher
  (#1251). A new `[i18n] locale_prefix_enabled` flag (default `false` — no
  behavior change for existing apps) makes every route registered via
  `AppBuilder::routes` also reachable under `/{locale}/...` for each
  configured `supported_locales`, with zero hand-duplicated route
  definitions: the router builds the content router once and nests a cheap
  clone under each locale prefix. An unknown `{locale}` segment (e.g.
  `/zz/posts`) 404s rather than panicking, and a request to the bare,
  non-prefixed path 308-redirects to the negotiated locale's prefixed path,
  preserving the query string. Within a locale-prefixed request, the URL
  segment now takes precedence over cookie/session/`Accept-Language` for the
  existing `Locale` extractor — with no handler changes required. New
  `[i18n] locale_prefix_exclude` config exempts route prefixes (e.g. `/api`,
  `/actuator`) from both localization and the bare-path redirect, so machine
  endpoints stay unprefixed. Two new view helpers,
  `autumn_web::widgets::{localized_path, locale_switcher}`, render
  path-and-query-preserving links to the current page in every supported
  locale. The SEO toolkit gained `SeoMeta::hreflang_alternates` plus a
  `seo::locale_alternates` helper that builds `<link rel="alternate"
  hreflang="…">` tags (including `x-default`) for a page's localized
  variants, and `sitemap.xml` now lists one entry per supported locale for
  each eligible static route when locale-prefix routing is enabled. The
  `examples/blog` i18n demo (`/greet`, plus the site-wide nav) was extended to
  exercise all of the above end-to-end.
- **slug:** a public `autumn_web::slugify(&str) -> String` helper (#1260) —
  lowercases, best-effort ASCII-folds accented Latin characters
  (`"café"` -> `"cafe"`), treats everything else as a separator, collapses
  runs to a single `-`, and falls back to a stable non-empty token for input
  that slugifies to nothing. `autumn generate scaffold`/`model` gain a
  `slug:slug{from:col}` DSL token that composes with the existing `unique`
  (#1032) and `references` (#1026) machinery rather than a parallel system: a
  `NOT NULL` column with its own `UNIQUE INDEX`, a free `find_by_slug`
  repository lookup, create-time auto-derivation from the named `from` field
  with a deterministic `-2`/`-3` collision suffix on a blank submission, and
  slug-keyed `show`/`edit`/`update`/`delete` HTML routes and generated links
  (`GET /posts/{slug}` instead of `GET /posts/{id}`) with a 404 on miss. A
  model supports at most one `slug` field; the combination with
  `--live`/`--live-validation`/`--sharded`/an `Attachment` field/a
  `:states(...)` field is rejected at generate time rather than silently
  emitting `id`-keyed routes. A non-slug scaffold's generated output is
  unaffected. `examples/blog`, `examples/wiki`, and `examples/reddit-clone`
  now use the shared helper instead of their own hand-rolled duplicates.

- **consent:** `autumn new` now scaffolds a cookie-consent banner and a real
  consent gate, so a fresh app is cookie-compliant by default (#1214). The new
  `autumn_web::consent` module provides a `Consent` extractor
  (`consent.allows("analytics", POLICY_VERSION)`) plus `accept_all_cookie` /
  `reject_non_essential_cookie` builders for a first-party cookie that records
  the chosen categories, a policy version, and a timestamp; bumping the app's
  policy version constant invalidates prior consent and re-shows the banner.
  The scaffolded banner (offering "Accept all" and "Reject non-essential" with
  equal visual weight) is injected automatically into every HTML page via
  `inject_consent_banner`, needs no JavaScript, and is wired into the base
  `autumn new` template alongside `POST /consent/accept` /
  `POST /consent/reject` routes (redirecting back to the referring page, not
  always the homepage) and a `GET /consent/manage` withdrawal route linked
  from the footer (GDPR Art. 7(3): withdrawing consent must be as easy as
  giving it). Strictly-necessary cookies (session, CSRF) are never routed
  through the gate — they keep being set unconditionally. The consent cookie
  reader defends against cookie tossing the same way the session cookie
  reader does, and injecting the banner (which embeds a live per-visitor CSRF
  token) marks the response `Cache-Control: private, no-store` /
  `Vary: Cookie` so it's never shared across visitors by a cache. An oversized
  HTML response is served intact rather than emptied, conditional-request
  headers are stripped while a prompt is pending so an `EtagLayer` can't
  short-circuit to a banner-less cached `304`, the CSRF cookie/form-field
  names are explicit parameters (`DEFAULT_CSRF_COOKIE_NAME` /
  `DEFAULT_CSRF_FORM_FIELD`) rather than read off request state, an internal
  `autumn build` / ISR render is passed through untouched instead of baking
  the banner into the static file on disk, and the banner uses
  `position: sticky` so it always reserves its own real, responsive height
  instead of a fixed CSS estimate. Additive; the `--api` JSON-first scaffold
  is unaffected (no HTML layout to show a banner in).
- **rich text:** a safe first-class path for **user-submitted** formatted text,
  so a content app built on Autumn cannot ship stored XSS by using the shipped
  helpers (#1255). `autumn generate scaffold post title:String body:richtext`
  is the whole setup: a `TEXT` column holding the Markdown **source**, a form
  with a Markdown editor and an htmx live preview, and a show view that renders
  through a sanitizer — no JavaScript of your own and no sanitizer to
  configure. The renderer is `autumn_web::markdown::render_user_content`, and
  it applies two independent controls: raw-HTML passthrough is disabled at the
  parser (raw markup becomes escaped text, and link/image destinations are
  checked against a URL-scheme allowlist before the HTML writer runs), and the
  resulting HTML is then run through an `ammonia` allowlist built from the
  curated `RICH_TEXT_ALLOWED_TAGS` / `RICH_TEXT_ALLOWED_URL_SCHEMES` public
  constants. Either control alone blocks the canonical payloads; both are
  applied so a bypass of one is not a bypass of the feature. The allowlist is
  deliberately tight — no `<img>` (a Markdown image degrades to its alt text,
  so a post cannot beacon a reader's IP to a third-party host), no `id`/`name`
  (DOM clobbering), `style` narrowed to table alignment, `class` narrowed to
  the code-fence language hint, and `rel="noopener noreferrer nofollow"` forced
  onto every surviving link. Block nesting is capped at 100 levels: the HTML
  sanitizer walks its open-elements stack once per block start tag, so
  uncapped nesting would be quadratic — and `"> "` is two source bytes per
  level, which would let one request body (or one stored post, re-rendered on
  every view) burn minutes of CPU. This is the counterpart to the existing
  `markdown::render`, which stays what it always was — a *trusted*,
  build-time renderer that injects heading anchors and applies no allowlist;
  its docs now say so and point here. `form::rich_text_area` renders the
  editor, `form::rich_text_area_htmx` adds the preview pane (filtering the
  one-time submit token out of the preview POST), and `sanitize_user_html`
  applies the same allowlist when the untrusted rich text arrives as HTML
  rather than Markdown. A 40-payload adversarial corpus locks the guarantee
  down structurally — it parses the rendered output and asserts that no
  non-allowlisted element, no event-handler attribute, and no URL outside the
  scheme allowlist survives. The generated preview endpoint reads only the
  field it renders, via the new `form::field_from_urlencoded` — the editor
  `hx-include`s the whole form, so decoding the form struct would fail on the
  first strictly-typed empty column of a freshly-opened `new` page and leave the
  preview blank until every unrelated field was filled in. A non-nullable
  `richtext` column renders through `required_rich_text_area*`, matching every
  other non-nullable generated control — `String` and `TEXT NOT NULL` both
  accept `""`, so without that signal an empty editor persisted a blank body.
  See the [rich text guide](docs/guide/rich-text.md). `form::rich_text_area` renders a labeled
  textarea, a minimal Markdown formatting toolbar, and the current value; the
  toolbar shows each construct's syntax rather than inserting it on click,
  because inserting into a `<textarea>` needs JavaScript and a control that
  silently does nothing without scripting is worse than none — the editor's
  contract is that it works with no JavaScript at all.
- **search:** a new optional plugin crate, `autumn-search`, turning the in-core
  full-text primitives (#842) into a **search subsystem**: mark a model
  searchable and get an index that stays in sync with the record lifecycle,
  plus semantic / vector retrieval, behind one engine-agnostic API (#1191).
  The `#[searchable]` attribute you already write is the single source of
  truth — `#[model]` now also derives an engine-agnostic `IndexDefinition`
  (index name, language, weighted fields) and a per-record `SearchDocument`
  from it, and a new `#[searchable(embed)]` field flag nominates the one field
  whose text is embedded for "find similar" / RAG retrieval. Two `embed`
  fields, or `embed` without the model-level `#[searchable]`, are compile
  errors that say so rather than a silent last-wins. Index sync is
  `SearchSyncHooks`, a ready-made `MutationHooks` you name once on the
  repository: create/update/delete then enqueue a durable `#[job]` reindex, so
  indexing is off-request and survives a restart, and repeated writes to one
  record coalesce into a single pending job. The payload is `(index, id)` and
  **not** the record — the handler re-reads the row, so a present row upserts
  and an absent row deletes. That single idempotent operation makes
  at-least-once delivery safe and lets a lost delete event, a soft delete, or a
  row changed by direct SQL repair itself. The dedup key is released when a job
  *starts* (so a write landing mid-reindex is never swallowed), and a
  concurrency cap of one **per record** keeps the two jobs that implies from
  interleaving a stale read over a newer write. Queries reuse `Page`/`ListQuery`
  (`search`, `search_list`, `search_hydrated`, `similar`, `similar_to`), and
  `autumn search reindex [--index NAME] [--purge] [--profile NAME]` rebuilds an
  index by running the application binary — the same technique `autumn jobs manifest` uses,
  because only the app knows which models, backend, and embedder are
  registered. A one-shot reindex against a profile with
  `enabled = false` exits non-zero rather than reporting a successful rebuild
  of nothing. `--profile` matters there: that binary resolves its own
  `[search]` section, and the CLI builds a debug binary, which core reads as
  `dev` — so a production rebuild must say so or it rebuilds the development
  index and reports success. Backfill walks the source by keyset (`WHERE <key> > $after`),
  never `OFFSET`, so a live table cannot skip or repeat rows, and it stops only
  on an empty batch — `scan` returns *up to* `limit`, so a source that filters
  after reading yields short batches with rows still behind them. Backends are pluggable
  behind a `SearchBackend` trait shaped so an external engine (Meilisearch,
  Tantivy, a vector store) is a new `impl` rather than a breaking change: query
  and result types are `#[non_exhaustive]`, capabilities are declarable from
  outside the crate, and a keyword-only engine is a complete implementation.
  The Postgres backend ships first, reusing
  `to_tsvector`/`setweight`/`ts_rank_cd` — with the framework's own weight
  array rather than Postgres' default, which differs at `B` and would otherwise
  rank a body-field match differently from every other backend — and adding
  `pgvector` when the
  extension is present — with a portable `double precision[]` +
  `autumn_search_cosine()` fallback so a managed Postgres without `pgvector`
  still boots and still answers k-NN queries. A `MemorySearchBackend` gives
  complete keyword *and* vector coverage with no Docker and no network, and
  doubles as the executable specification for the backend contract.
  Embedding is pluggable via an `Embedder` trait; the crate ships **no** model,
  runtime, or vendor SDK — only a `NoEmbedder` that refuses (so a missing
  provider is a typed error, never invented vectors) and a deterministic
  `HashingEmbedder` for dev and tests.
  Search respects existing authorization, and enforces it rather than advising
  it: a `SearchVisibility` hook turns a `PolicyContext` into a `SearchFilter`
  that is a *required argument* of the backend query methods, so page totals
  and k-NN neighbour counts are computed after the restriction rather than
  before; filters **intersect**, so a caller can only narrow what authorization
  allowed, and two incompatible constraints collapse to "match nothing" rather
  than to an arbitrary winner; a failing hook aborts the search instead of
  widening it; calling the authorization-aware entry point with no hook
  registered is a typed error rather than an unfiltered read; and `similar_to`
  filters the *seed* read as well as the neighbour query, so "more like this"
  cannot become an inference channel over records the caller may not see. A
  model with a `tenant_id` column marks its index tenant-scoped, and querying
  one with no tenant in scope is refused — matching
  `#[repository(tenant_scoped)]`, and closing the case where a search route
  mounted outside the tenancy layer would otherwise have returned every
  tenant's rows. Because a `SearchFilter` is plain data, an out-of-scope engine
  gets the same tenant/visibility restriction.
  Query text is a bag of words across every backend — each token must match,
  operator syntax (`OR`, `field:`, `*`) is never interpreted, a blank query
  returns an empty page having issued no query, and a filter key that is not a
  declared field never reaches SQL — which keeps results consistent between
  engines and makes a hostile query string structurally incapable of widening
  the result set. `[search]` is resolved through the same profile layering the
  runtime uses — base `autumn.toml`, then `[profile.<name>.search]`, then
  `autumn-<profile>.toml`, then `AUTUMN_SEARCH__*` env vars — so
  `enabled = false` is a working incident switch *per environment*: it stops
  index writes (including a purging backfill) without failing writes to the
  model, and cannot be set in prod only to be ignored there. The declared
  `embedding_dimensions` is checked against the installed `Embedder` at
  startup and a disagreement refuses the boot, because the alternative is a
  silent one: writes keep succeeding while the vector column rejects every
  value and semantic search returns nothing. The index definition also carries
  the model's real key column, so a `#[id] pub note_id: i64` over a legacy
  table that still has an unrelated `id` backfills off the right one. The
  section is read through core's own `Env` abstraction, so the macro-supplied
  crate directory and build mode are visible and a release binary resolves the
  same profile core does. A `deleted_at` column is not by itself a tombstone:
  the source follows the repository's `soft_delete` opt-in, so audit-history
  rows stay indexed exactly as the model's finders return them.
  Request-controlled values — pagination, the k-NN minimum score, the query
  vector's width — are bound rather than formatted into the SQL, because
  diesel's statement cache is keyed on query text and never evicts. `[search]`
  is resolved through core's `.env` overlay too, so a kill switch set there
  takes effect. A *filtered* k-NN query skips the ivfflat index deliberately:
  it probes candidate lists before the `WHERE` runs, so a selective
  authorization filter could otherwise return short or empty while qualifying
  neighbours sat in unprobed lists. A disabled subsystem initializes
  nothing — no `ensure_index`, no DDL, no width check — so a search outage
  cannot abort application startup after the switch has been thrown. A backfill
  takes a write watermark up front and never overwrites — or re-creates — a
  record a concurrent reindex touched after it started, so the bulk and
  per-record writers converge instead of racing; deletes are recorded in a
  ledger that outlives the document — written in the same statement that removes
  it and cleared in the same statement that re-creates the record, so no
  concurrent write can observe half of either — and a mid-backfill delete
  cannot be undone by a stale batch. Tenant scoping, like soft delete, follows the repository's
  opt-in rather than the mere presence of a column. The in-core
  `#[repository(searchable)]` `search()` and its `websearch_to_tsquery`
  semantics are untouched; this subsumes #842 as one backend, it does not
  replace it. See `docs/guide/search.md`.

- **seo:** route-level meta tag defaults via a `seo(...)` route attribute
  argument, closing the acceptance criterion deferred from the SEO toolkit
  (#1182, deferred from #830). Static per-page metadata no longer has to be
  rebuilt by hand in every handler: declare it once on the route —
  `#[get("/about", seo(title = "About • My Blog", description = "Learn about
  us"))]` — and take a `SeoMeta` parameter, which now implements
  `FromRequestParts` and arrives pre-populated with the declared values. The
  extractor is infallible, so a handler on a route that never mentions
  `seo(...)` simply receives an empty builder; the builder is consuming as
  before, so a handler refines the defaults with per-request data
  (`seo.title(format!("{} • Blog", post.title))`) and its value wins for the
  keys it touches while the untouched attribute keys survive. Every `SeoMeta`
  builder method has a matching key (`title`, `description`, `canonical`,
  `og_title`, `og_description`, `og_image`, `og_type`, `og_url`,
  `twitter_card`, `twitter_title`, `twitter_description`, `twitter_image`,
  `robots`); a typo'd, repeated, or empty `seo(...)` is a compile error naming
  the supported set, rather than metadata that silently never renders.
  `#[static_get]` accepts the same argument, so pre-rendered pages carry the
  tags too — static generation drives the same router, so no separate wiring
  was needed. A static route declaring `robots = "noindex"` is now also left
  out of the generated `sitemap.xml`, so Autumn no longer advertises a URL it
  derived itself while also asking crawlers not to index it. This covers the
  paths Autumn derives from `#[static_get]` routes; entries from a
  `SitemapSource` you register are an explicit, application-authored URL list
  and are passed through unfiltered (a `SitemapEntry` carries only a `loc`,
  with nothing tying it back to a route). `#[ws]` is the one route macro
  that rejects `seo(...)` — a WebSocket upgrade serves no crawlable document —
  and says so rather than failing with a bare parse error. The
  declared values are recorded on the new `Route::seo` field as a `Copy`,
  `&'static str`-backed `SeoRouteDefaults`, and the router installs the request
  extension only for routes that declared something, so routes without
  `seo(...)` pay nothing. As before, the attribute supplies *values*, not
  markup: handlers still decide where to emit them, normally via
  `SeoMeta::render()` inside a layout. **Breaking:** code that constructs
  `autumn_web::Route { .. }` or `autumn_web::static_gen::StaticRouteMeta { .. }`
  literally (plugins building a `Vec<Route>` by hand rather than through
  `routes![]`) must add `seo: autumn_web::seo::SeoRouteDefaults::EMPTY`. See the
  [migration guide](docs/migrations/0.7.0.md).
  `SeoRouteDefaults` is itself `#[non_exhaustive]` and built by chaining its
  `const fn with_*` setters from `EMPTY`, so future SEO keys stay additive.
- **cli:** `autumn console` (alias `autumn c`) — a one-command, pre-wired data
  playground (#1039). Autumn's answer to `rails console` / `manage.py shell` /
  `iex -S mix`: because Rust has no stable `eval`, it follows loco.rs's
  edit-and-run model rather than building an interpreter. The first invocation
  scaffolds `src/bin/playground.rs` already wired with the same config and
  database-URL resolution `autumn seed`/`autumn dev` use
  (`AUTUMN_DATABASE__PRIMARY_URL` → `AUTUMN_DATABASE__URL` → `DATABASE_URL` →
  `autumn.toml`, profile-aware, via `autumn_web::seed::SeedContext`), a
  constructed async pool (`ctx.pool()`), a checked-out connection (`db`), and a
  clearly-marked `// your code here` region; every invocation compiles and runs
  it against the resolved environment. Because a Cargo binary target is its own
  crate and a generated app has no `src/lib.rs`, the playground declares the
  app's `schema`, `models`, `repositories`, and `policies` modules with
  `#[path]`, so a `find_all()`/`find_by_id()` round-trip compiles with no
  further wiring. The playground's `[[bin]]` is gated behind
  `required-features = ["playground"]`, so `cargo build`, `cargo test`, `autumn
  dev`, and `autumn build` skip it entirely and only `autumn console` ever
  compiles it — a playground that doesn't compile can never break the app's
  default build, and the `seed` feature's implied `db` never reaches a
  DB-free project's normal builds. Re-running never overwrites an edited
  playground (`--force` regenerates from the template, and refuses to follow a
  symlink); `--scaffold-only` stops before the build. The two `Cargo.toml`
  edits — the `playground` feature and the gated bin target — go through a
  format-preserving TOML editor, are written atomically, never touch the
  `autumn-web` dependency line, and are idempotent. A config or database
  failure prints the underlying error and exits non-zero from the playground
  out through the command's own exit status. Because that isolation is a
  guarantee rather than a best effort, `autumn console` refuses — before
  writing anything, leaving `Cargo.toml` byte-identical — when a manifest is in
  a state where it cannot hold: an existing `playground` bin target that is
  ungated or gated on a different feature, a `default` feature list that
  reaches `playground` (directly or transitively), or an edition-2015 package
  where the scaffolded file would be auto-discovered as an ungated binary. Each
  error names the one line to change. Guide: `docs/guide/console.md`.
  No `autumn-web` API change.
- **notifications:** first-class in-app notifications store with a read/unread
  feed (#1148). A new `autumn_web::notifications` module ships a
  `Notifications` service/extractor (surfaced like `Session`/`Auth`) with
  `notify(recipient_id, kind, payload)`, `list(...)` returning the shipped
  `Page`/`ListQuery` pagination (incl. `filter[unread]=true` and
  `filter[kind]=...`), `unread_count`, and idempotent `mark_read` /
  `mark_read_for` (recipient-scoped) / `mark_all_read`. Storage is a pluggable
  `NotificationStore` — `DbNotificationStore` (Postgres or SQLite) by default
  when a pool is configured, `MemoryNotificationStore` for DB-less dev/tests,
  or a custom store via `AppBuilder::with_notification_store`. With the `ws`
  feature, `notify_with_push` additionally publishes the stored notification
  JSON on the per-recipient `notifications:{id}` channel topic, best-effort (a
  channel failure never fails the notify). A new `autumn generate
  notifications` command scaffolds the backend-aware migration, a minimal feed
  module with registered routes, and an in-process `TestClient` smoke test.
  Guide: `docs/guide/notifications.md`. All additions are additive — no
  breaking change to existing surfaces.
- **cli/generate:** `autumn generate auth` and `autumn generate mailer
  --list-unsubscribe` are now **backend-aware on SQLite** (#1927). On a SQLite
  app they scaffold SQLite-dialect migrations — `INTEGER PRIMARY KEY
  AUTOINCREMENT`, `DEFAULT CURRENT_TIMESTAMP`, and `INTEGER` foreign keys, across
  the users/sessions/remember-token tables and the optional `--totp`,
  `--magic-link`, `--oauth`, `--passkeys`, and `mail_unsubscribes` tables —
  instead of being rejected at generate time. Postgres output is byte-for-byte
  unchanged.
- **cli/generate:** the generated `autumn generate auth` **DB-backed session
  store now works on SQLite** (#1908): its connection pools are typed against
  `::autumn_web::RuntimeConnection` (resolving to `AsyncPgConnection` by default
  and the SQLite connection under the `sqlite` feature) rather than a hard-coded
  `diesel_async::AsyncPgConnection`, so the generated app compiles on whichever
  backend it selected.
- **generate/sqlite:** `DateTime<Utc>` and `Attachment` model fields now compile
  and round-trip on the SQLite backend, reaching Postgres parity (#1924). A
  `DateTime<Utc>` column maps to diesel's `TimestamptzSqlite` sql-type, stored as
  an RFC 3339 UTC string (SQLite `TEXT` affinity) that sorts and compares
  chronologically; an `Attachment` (`autumn_web::storage::Blob`) column stores
  its metadata JSON in a `TEXT` column via new local `FromSql`/`ToSql<Text,
  Sqlite>` impls on `Blob`. Both kinds are now accepted by `generate model` /
  `generate scaffold` / column migrations on a SQLite app instead of being
  rejected at generate time. `Uuid` and `Decimal` remain rejected on SQLite
  (their `uuid::Uuid` / `rust_decimal::Decimal` types are foreign to autumn-web,
  so the orphan rule blocks a direct SQLite conversion, and diesel /
  `rust_decimal` provide only Postgres-side impls) — a wrapper-based follow-up
  tracked under #1924.
- **docs:** a **feeds** guide (`docs/guide/feeds.md`) documenting Atom/RSS feed
  generation, cross-linking the runnable `examples/blog` `/feed.xml` route
  (#2099). [no-plugin]
- **docs/examples:** a runnable state-machine / lifecycle demonstration (`#[state_machine]` transition effects) in the `wiki` example — the `Page::status` machine now declares per-edge `on = "..."` effects that append the audit `Revision` inside the transition's transaction, driven from the transitions handler via `transition_status_to_on_conn` under one `Db::tx_with` — cross-linked from the state-machines guide (#2099). [no-plugin]
- **docs:** an **aggregate queries** guide (`docs/guide/aggregates.md`) documenting GROUP BY roll-ups, paired with a runnable `GET /stats` roll-up route in the `bookmarks` example (#2099). [no-plugin]
- **docs:** an **audit logging** guide (`docs/guide/audit-logging.md`) documenting audit events with actor auto-attribution, with a minimal audit sink wired into the `reddit-clone` example (#2099). [no-plugin]
- **docs:** a core **authentication** guide (`docs/guide/authentication.md`) —
  the session-auth hub that was previously rustdoc-only: the `Session`
  extractor and its store backends, `[session]` cookie configuration, password
  hashing and the `[auth.password]` policy (weak-list, context similarity, HIBP
  k-anonymity), login/logout anatomy (session-id rotation, non-enumeration),
  `#[secured]`/`RequireAuth`/`Auth<T>`, `[auth.lockout]`, rotating remember-me
  tokens with theft detection, active-session revocation, `acting_as` in tests,
  and a production checklist — cross-linked from the OAuth, step-up,
  authorization, and testing guides and from the `saas`/`reddit-clone` login
  handlers (#2099). [no-plugin]
- **docs:** an **OpenAPI generation** guide (`docs/guide/openapi.md`) — the
  spec pipeline was previously rustdoc-only despite the runnable `bookmarks`
  example: what the route macros infer from a handler signature (path params,
  `Query<T>`, `Json<T>`/`Valid<Json<T>>` bodies, `Vec`/`Option`, tuple and
  `Result` returns), the full `#[api_doc(...)]` key table and its attribute
  ordering rules, where component schemas come from (`#[model]`,
  `#[derive(OpenApiSchema)]`, `register_schema`, and the placeholder fallback)
  plus collision-resolved component keys, the shared `ProblemDetails` error
  responses, `SessionAuth`/`BearerAuth` derivation from `#[secured]`, version
  deprecation/sunset in the spec, scoped-group paths, the `[openapi]` profile
  gate and tenancy `public_paths` note, `autumn build`'s
  `dist/openapi.{json,yaml}` export (and the static-routes prerequisite that
  gates it), and spec assertions with `TestApp` —
  cross-linked from the MCP, API-versioning, routes-CLI, and macro-transparency
  guides (#2099). Also corrects stale rustdoc that advertised a `/v3/api-docs`
  default (the served default is `/openapi.json`) and an "OpenAPI 3.0" document
  (the generator emits 3.1.0). [no-plugin]
- **sim-testing:** add the **seed-sweep runner** (`sim::sweep`, W6 PR3,
  #1797) and the CI-facing `sim-sweep` `[[bin]]`: `sweep_proptest(seeds,
  &strategy, body)` runs `Sim::run_proptest` sequentially across a batch of
  seeds, stopping at the first failing seed and reporting its shrunk
  op-sequence. `SweepFailure`'s `Display` is caller-agnostic (it doesn't
  prescribe a replay command, since `sweep_proptest` has no idea what test or
  binary is calling it); the `sim-sweep` bin appends its own replay
  suggestion when it prints a failure, since it knows its own invocation.
  Folds every proptest case's `sometimes!` observations (not just the last of
  up to 256 cases per seed — `Sim::run_proptest_with_case_hook` is a new
  `pub(crate)` hook for this) into a cross-seed aggregate, so a fully-green
  sweep is only reported as `Passed` when it is also non-vacuous (`Vacuous`
  otherwise, if some label was observed but never satisfied anywhere in the
  range). Deliberately sequential rather than parallel across a worker pool:
  review of an earlier revision surfaced that `TestApp::build` (what a
  `body` mounting a real app calls) unconditionally touches process-global
  state (the cache, the event bus, and — when jobs are configured — a global
  job client), which concurrent workers would race on, and that
  `Sim::run_proptest`'s sync-only `body` signature can't `.await`
  `Sim::run_to_idle` to drain spawned background work even with a runtime
  entered — both architectural, not patchable within this sweep. Sequential
  execution sidesteps both; sweep-level threading was orthogonal to the
  harness's actual concurrency-bug-finding power anyway (that comes from
  exploring seeds against W1's single-threaded deterministic executor, not
  from how many OS threads process the outer seed range). An empty seed range
  (`AUTUMN_SIM_SEEDS=0`, or any empty iterator passed to `sweep_proptest`)
  reports the new `SweepOutcome::Empty` rather than silently falling through
  to `Passed { seeds_run: 0 }` — the bin treats it as a failure (exit `1`) so
  a misconfigured seed count can't quietly green the CI job without testing
  anything. `embedded_config()` (the op-driver's internal proptest `Config`,
  already forcing fork/timeout off regardless of ambient `PROPTEST_FORK`/
  `PROPTEST_TIMEOUT`) now also pins the case count to a fixed 256, ignoring
  `PROPTEST_CASES` entirely: `PROPTEST_CASES=0` would otherwise make a case
  closure never run at all while still reporting success, and even a bare
  "clamp to at least 1" would still collapse proptest's automatic shrink
  budget (`cases × 4`) to 4 iterations, aborting shrinking early with a
  far-from-minimal counterexample. `AUTUMN_SIM_SEEDS=1000 cargo run -p
  autumn-web --release --features sim-testing --bin sim-sweep` sweeps seeds
  `0..1000` against a built-in account demo scenario; a new standalone CI job
  runs it at seed count 512 on every push/PR, structured like the `loom`
  job. [no-plugin]
- **sim-testing:** add a property-based **op-driver** (`sim::op`, W6 PR2,
  #1797) behind the new `sim-testing` feature: `Sim::gen_ops::<T>()` /
  `Sim::gen_ops_with(strategy)` deterministically draw an arbitrary `Vec<T>`
  of app-defined operations from a seed, and `Sim::run_proptest(seed,
  strategy, body)` is the shrink-capable entrypoint — it owns a proptest
  `TestRunner` and rebuilds a fresh `Sim::from_seed(seed)` for every case
  (including every shrink attempt), so an `always!` violation shrinks to a
  minimal, byte-for-byte-reproducible counterexample. The op-generation RNG
  stream is salted independently of the app-facing entropy, chaos, and crash
  streams, and proptest's own file-based failure persistence is disabled in
  favor of the `AUTUMN_SIM_SEED=…` replay line. `proptest` is now also an
  optional **library** dependency (not just dev-only), so this can live in
  `src/` and the forthcoming `sim-sweep` binary (W6 PR3) can depend on it
  too. [no-plugin]
- **sim-testing:** add the `always!` / `sometimes!` simulation assertion macros
  (W6 op-driver assertion core, #1797) — hard-invariant + reachability assertions
  for `#[sim_test]`, with a non-vacuity registry the forthcoming sim-sweep
  aggregates. `always!(cond[, "fmt", …])` panics on a false invariant with a
  greppable message (the `#[sim_test]` harness prints the `AUTUMN_SIM_SEED=…`
  replay line); `sometimes!(cond, "label")` records a reachability target in a
  thread-local registry (observed vs satisfied) that `Sim::from_seed` resets per
  seed, exposed via `sometimes_snapshot` / `assert_all_sometimes_satisfied` for
  the sweep to fail a green-but-vacuous run. [no-plugin]

- **docs/examples:** documented the `autumn-media-plugin` media subsystem
  (broadcast + mesh rooms + MediaMTX). Adds the `docs/guide/media.md` guide
  (install/mount, `MediaConfig` profile loading, the `RoomService`/`RoomStore`
  rooms surface incl. the `memory` vs multi-process `db` backend,
  `MediaMtxClient`/`MediaUrls`, and the durable encode jobs / `MediaArtifactSink`)
  and a new runnable `examples/media-room` crate that installs the plugin with
  rooms and serves create/join/list routes, wired into the example drift gate
  (workspace member, EXAMPLES.md catalog entry, README table, quickstart
  README). [no-plugin]

- **docs:** a **content negotiation** guide (`docs/guide/content-negotiation.md`)
  documenting the `Negotiate` extractor and its `.respond(html, json)` responder —
  one handler serving HTML to browsers and JSON to API clients from a single
  source of truth, including `Accept` q-value precedence, `q=0` exclusions, the
  `406 Not Acceptable` arm, and `default_format`. Paired with a runnable
  `GET /todos/summary` dual-render route in the `todo-app` example (#2099).
  [no-plugin]
- **docs/examples:** a `docs/guide/nested-forms.md` guide for nested (`has_many`)
  form binding (`NestedChangesetForm<P, C>`, `NestedChild`, `inputs_for`,
  `_destroy`, atomic saves) plus a runnable master–detail form in
  `examples/wiki` — the new **Collections** feature, where a collection (parent)
  owns many links (children), created/edited/removed in one transaction-backed
  form. `[no-plugin]`
- **docs:** a `docs/guide/downloads.md` guide covering the typed `Download`
  response — the `from_bytes` / `from_stream` / `from_async_read` / `from_blob`
  constructors, the `.filename` / `.content_type` / `.inline` / `.etag` /
  `.last_modified` builders, and `into_response_ranged` for RFC 7233 `Range`
  requests / `206 Partial Content` — plus a range-capable CSV export route
  (`GET /bookmarks/export.csv`) added to the `bookmarks` example. `[no-plugin]`
- **docs:** a `docs/guide/submit-tokens.md` guide for one-time submit tokens
  (at-most-once form submissions) — the double-submit / duplicate-POST problem,
  how the default-on `SubmitTokenLayer` + hidden `_submit_token` field +
  `SubmitToken` extractor close it with no client JS, how it differs from CSRF
  and `Idempotency-Key`, and the `[security.submit_token]` config knobs. The
  `saas` example now guards its signup POST with a one-time submit token so a
  double-clicked signup cannot create a duplicate account. [no-plugin]
- **sim-testing:** the `Sim` chaos harness gained a **crash / restart** primitive
  and a seed-derived crash schedule for durable crash-recovery tests (#1797, W5.c
  item 7). `Sim::kill()` drops the mounted app so the in-process job runtime's
  in-flight work is cancelled **without** completing (modelling a process crash),
  `Sim::restart(TestApp)` mounts a fresh app on the **same durable database**, and
  `Sim::crash_and_restart(TestApp)` is the kill-then-restart convenience. A new
  `sim::crash` module derives a reproducible `CrashSchedule` / `CrashPoint` from a
  dedicated `seed ^ CRASH_STREAM_SALT` stream (independent of the app-facing
  entropy and the chaos decision stream), read through `Sim::crash_schedule()` /
  `Sim::crash_point()`, so two same-seed runs replay an identical crash schedule.
  The Definition-of-Done (`sim_chaos_crash`) commits a **real durable repository
  commit hook** to the DB-backed `autumn_repository_commit_hooks` queue on the
  in-memory `SQLite` substrate, kills the sim before it drains, restarts on the
  same `substrate.pool()`, and proves `run_to_idle()` recovers and runs the hook
  exactly-once/idempotently. This wave injects a **single representative
  deterministic crash point** (after a repository write has enqueued its durable
  hook but before it drains) with the schedule API shaped generally — documented
  representativeness, not faked generality. The `sqlite` substrate runs the
  in-memory `local` job backend, which is **not durable**: a kill drops its
  mid-flight jobs by design, so the durable guarantee is asserted against the
  commit-hook queue, not the local job queue. Additive: no existing signature
  changes; a default/no-crash `build` + `run_to_idle` is byte-for-byte unchanged.
  [no-plugin]
- **test-support:** `autumn_web::test::drain_ready_repository_commit_hooks(pool, max_rows)`
  deterministically claims and runs ready durable repository commit hooks in
  integration tests — driving the real worker→drain wiring without starting the
  timing-based background commit-hook worker — and returns the number processed.
- **sim:** a seeded **`Entropy`** seam for deterministic identifiers (#1797),
  mirroring the `Clock`/`ClockSource` seam. `AppState` now carries an injectable
  entropy source (`OsEntropy` by default; `SeededEntropy` over `ChaCha8Rng` for
  simulation), reachable in handlers through the new **`Rng`** request extractor
  (`autumn_web::entropy::Rng`) and overridable via `AppState::with_entropy` /
  `TestApp::with_entropy`. Deterministic `uuid_v4` / `uuid_v7` helpers, plus a
  seed-derived, order-independent `derive_uuid(purpose_tag)` namespace helper
  (on `SeededEntropy` and `SimRng`) for byte-reproducible multi-tenant fixtures,
  round out the surface. The framework's four high-value id sites — job ids,
  request-id middleware, idempotency in-flight lock owners, and session ids —
  now mint through this source, so under a fixed seed the whole identifier
  stream replays byte-for-byte. Other `Uuid::new_v4()` sites are intentionally
  left as-is (a crate-wide deny-lint is deferred to a later phase).
- **sim-testing:** the `Sim` handle gained its W2 virtual-clock + drain wiring
  (#1797): `Sim::build(TestApp)` mounts an app on the paused runtime with the
  simulation's `TickingClock` installed (via `TestApp::with_clock`), exposing the
  resulting `TestClient` through `Sim::client()` / `try_client()`;
  `Sim::advance(dur)` steps the injected wall clock and tokio's paused timer wheel
  together so `Utc::now()`-via-extractor and `tokio::time::sleep` stay in
  lockstep; and `Sim::run_to_idle()` cooperatively drains ready jobs and
  timer-woken work to quiescence. A job whose retry backs off 24h now fires in
  virtual time with zero wall-clock sleep. [no-plugin]
- **sim-testing / rate-limit:** the built-in token-bucket rate limiter now reads
  its refill clock from the framework's injected `ClockSource` instead of
  `Instant::now()` / `SystemTime::now()` (#1797). `RateLimitLayer::with_clock`
  threads `AppState`'s clock through the limiter, both bucket stores, and both
  construction sites (the global rate-limit middleware and the `#[throttle]`
  path); a per-key bucket's `last_refill` is stored as a `DateTime<Utc>` with a
  fail-safe clamp of negative wall-clock deltas to zero refill (mirroring the
  Redis Lua's `math.max(0, …)`). A `#[sim_test]` can now deterministically
  **exhaust** a bucket and then **refill** it under virtual time via
  `Sim::advance`, with zero real sleep. Production behavior under the default
  `SystemClock` is unchanged, and `rate_limit.lua` is untouched (the timestamp is
  still supplied as a Rust-computed `ARGV`). [no-plugin]
- **sim-testing:** `Sim::advance_to(target)` advances the virtual clock **to** a
  zoned instant (#1797), the DST/timezone-aware companion to `Sim::advance(dur)`.
  It is generic over any `chrono::TimeZone` (pass a `chrono::DateTime<Utc>`, a
  `FixedOffset` datetime, or a `chrono_tz::Tz` datetime — no new hard dependency)
  and resolves the target to the correct UTC instant before reusing `advance`
  internally, so the injected clock and tokio's paused timer wheel stay in
  lockstep across a DST spring-forward boundary and any timer due inside the
  crossed window still fires. Time is forward-only: advancing to the current
  instant is a no-op and advancing to a strictly-past instant panics. A companion
  `Sim::advance_to_local(naive, &tz)` resolves a naive wall time with explicit,
  deterministic DST-edge handling (ambiguous fall-back → earlier instant; spring
  -forward gap → carried across the gap), never `.unwrap()`ing a `LocalResult`.
  [no-plugin]
- **sim-testing:** wire the W4 `SQLite` sim DB lane through the W2 `Sim` API
  end-to-end (#1797). A new standalone integration test (`sim_sqlite_integration`)
  attaches a fresh, migrated, in-process in-memory `SqliteSubstrate` pool to a
  `TestApp` via `TestApp::with_db`, mounts it through the real public
  `Sim::build(app)`, and drives a 24h-backoff `#[job]` to completion purely via
  `Sim::advance` + `Sim::run_to_idle` — no `perform_enqueued_jobs`, no wall-clock
  sleep — then reads back (with real SQL) the row the job's successful retry wrote,
  proving the W2 virtual-clock drain runs against the W4 substrate over the
  representative in-process scheduler + local job-runtime paths. Additive: no
  changes to the `Sim`/`SimApp`/`SqliteSubstrate` public surface. `SqliteSubstrate`
  now applies the framework's `SQLite` repository-commit-hook migration set itself
  (before any caller migrations), so the `autumn_repository_commit_hooks`
  control-plane table always exists and `Sim::run_to_idle` no longer panics with
  "no such table" when draining an app mounted on a bare substrate — this test
  therefore registers only its own app migration, with no copied framework-DDL
  fixture to drift. [no-plugin]
- **sim-testing:** `Sim::strict_wall_clock()` (and `Sim::strict_wall_clock_budget(dur)`)
  add an opt-in **real-time leak guard** (#1797): with it enabled, `Sim::advance`
  / `Sim::run_to_idle` panic if a paused-sim step burns more than a budget of
  *real* wall-clock time (default 100 ms; overridable per run via the
  `AUTUMN_SIM_STRICT_WALL_CLOCK_BUDGET_MS` environment variable), catching a real
  `std::thread::sleep` / blocking I/O / `spawn_blocking` that escaped tokio's
  paused virtual timer. The panic flows through the `#[sim_test]` macro's
  `catch_unwind`, so the `AUTUMN_SIM_SEED=…` replay line still prints. This is a
  **runtime backstop for the worst off-seam pattern (a real blocking sleep), not
  off-seam-read detection** — a free-function `Utc::now()` / `Instant::now()` has
  no runtime interception point in safe Rust, so finding those reads stays the
  Phase-2 deny-lint's job; the two are complementary. Off by default; the field
  is a plain read-only `Option<Duration>` (no interior mutability), so the
  `&self` advance/drain futures stay `Send`. [no-plugin]
- **dev:** add `scripts/pre-push-check.sh`, a pre-push gate that mirrors CI's
  `lint` + `test` jobs (`cargo fmt --all -- --check`, `cargo clippy --workspace
  --all-targets`, a compile-only `cargo test --workspace --no-run`, and a
  `cargo test --workspace --doc` doctest leg). The compile-only step builds every
  workspace test target — including the autumn-web consolidated
  `integration_tests` binary that a narrow `cargo test -p autumn-cli` loop never
  links — so cross-package compile breaks (e.g. the #1614 sqlite+mail `E0308`)
  are caught locally instead of surfacing as CI "flakes". `--no-run` keeps it
  disk-cheap by skipping the trybuild run that expands scratch by ~17GB. Because
  `--no-run` does **not** build doctests (they compile only in the `--doc`
  phase) and cargo has no stable compile-only doctest mode, the separate
  `--workspace --doc` leg runs them to catch doctest breaks like #2107 (an
  `app.rs` `no_run` example that broke after a struct gained a field); it stays
  infra-free and disk-cheap because doctests are overwhelmingly `no_run`/`ignore`
  (still compiled, so the break is caught) and `--doc` never triggers trybuild.
  Documented in CONTRIBUTING.md "Before you push". [no-plugin]
- **media:** the mesh-room `RoomStore` seam (#1974) is now **async** and gained a
  shared, **multi-process-safe** database-backed implementation. The `RoomStore`
  trait, `RoomService`, and the four room HTTP handlers are now `async` (via
  hand-rolled boxed futures — `RoomStoreFuture` / `ReapFuture` — mirroring the
  crate's existing `MediaSinkFuture` seam, no `async_trait` dependency), so a
  networked/durable backing store can `.await` real I/O. A new
  `rooms_db::DbRoomStore` persists rooms and participants in two tables
  (`media_rooms`, `media_room_participants`), so rooms survive restarts and every
  process/instance sharing the database sees the same rooms. It is selected via
  the new `[media] room_store_backend = "memory" | "db"` config key (env
  `AUTUMN_MEDIA__ROOM_STORE_BACKEND`); **`memory` remains the default**
  single-process store. All queries are written against autumn-web's
  `RuntimeConnection` / `RuntimeBackend` aliases (Postgres by default, `SQLite`
  under `autumn-web/sqlite`) so **both lanes compile**; the crate never enables
  the `sqlite` runtime feature itself. The idle-room / stale-participant reaper
  works against the shared store via a **last-write-wins** design: `reap_stale`
  deletes stale participants and now-empty stale rooms with unconditional deletes
  keyed only on the injected clock, so **concurrent reapers across processes
  converge with no corruption** (no lease row, no leader election) while still
  never crossing namespaces. Ships a `20260720000000_media_rooms` migration for
  apps that opt into the `db` backend. (0.7.0 work — do not merge until v0.6.0 is
  cut.)
- **media:** the runtime `MediaConfig` loader is now **profile-aware** (#2066).
  New `MediaConfig::from_autumn_dir` / `from_autumn_dir_with_env` resolve the
  active profile (`AUTUMN_ENV` → `AUTUMN_PROFILE` → `--profile` →
  `AUTUMN_IS_DEBUG=0` ⇒ `prod` → `dev`) and merge the `[media]` subtree across
  the same layers Autumn's core config loader and the deploy CLI use — base
  `autumn.toml` `[media]` ← inline `[profile.<name>].media` ←
  `autumn-<profile>.toml` `[media]` — before applying the runtime's `${VAR}`
  interpolation and `AUTUMN_MEDIA__*` overrides last. Previously the runtime
  read only the top-level `[media]` table, so a `[profile.prod.media]` block
  (or a `[media]` table living only in `autumn-prod.toml`) was honored by a
  deploy but silently ignored at runtime under `AUTUMN_ENV=prod`. The existing
  single-file `from_autumn_toml` / `from_toml_str_with_env` entry points are
  unchanged.
- **teams:** a new `autumn generate teams` subcommand (#1261) scaffolds team
  membership for an existing app — organizations (tenants), a closed
  `Owner`/`Admin`/`Member` role per membership, and email invitations —
  entirely by composing already-stable primitives rather than introducing a
  new authorization mechanism: `#[repository(..., tenant_scoped)]` (#695)
  filters/stamps every `Membership`/`Invitation` read and write by the
  active organization, the session `"role"` key (#496, the same one
  `#[secured("...")]`/`PolicyContext::has_role` already read) backs the new
  `require_role` guard, and the Mail stack's `#[mailer]` sends the invite
  email. Unlike every other generator here it takes no name — it always
  emits the fixed `Organization`/`Membership`/`Invitation` set under
  `src/teams/`, plus a `migrations/<timestamp>_create_teams/` migration and
  the `mail` Cargo feature on `autumn-web`, wiring ten routes into
  `src/main.rs`'s `routes![...]`. It deliberately does not generate its own
  login/signup — your app's already exists — so the integration surface is
  two lines of hand-written code: call the new
  `teams::routes::organizations::provision_default_organization` at the end
  of your signup handler, and `teams::role::establish_org_session` after
  resolving the caller's active membership at login (see
  `docs/generate-teams.md`). A new `examples/teams` reference app (owning
  its own `users` table end to end, so it inlines both integration points
  directly into `routes/auth.rs`) is the fully-wired ground truth this
  generator adapts from.
- **`AppState::config_arc()`** — the cheap config accessor (#2198). It hands
  back the `Arc<AutumnConfig>` the extension map already holds, so reading
  configuration costs a refcount bump instead of a deep clone of every section.
  `config()` is unchanged — same signature, same owned and independently
  mutable snapshot, now expressed as `(*self.config_arc()).clone()` — and
  remains the right call when you need to own and mutate a snapshot; reach for
  `config_arc()` on anything that runs per request. Six framework read sites on
  request paths moved over (the `#[throttle]` limiter, the upload size check,
  the `TimeZone` extractor, both alert paths, and `TestClient`'s N+1 threshold
  read), each of which was paying ~65 heap blocks per call for a config it only
  read. When no config extension is installed — typically a test that doesn't
  wire the full startup pipeline — the accessor yields a handle to a shared
  process-wide default rather than allocating one, and never writes it back, so
  a config installed afterwards is still observed. A new isolated test binary
  (`autumn/tests/config_alloc_gate.rs`, on the new `allocation-counter`
  dev-dependency — it installs a counting `#[global_allocator]`, a
  process-wide side effect, so it cannot live in the consolidated suite) pins
  the accessor at exactly zero allocations on both the installed and fallback
  paths.

  The plugin's `api-reference.md` (*Config layering and env keys*) documents
  when to reach for which accessor.

- **router:** new `health.enabled` config knob (default `true`,
  `AUTUMN_HEALTH__ENABLED`) — an opt-out that suppresses all built-in probe
  endpoints (`/health`, `/live`, `/ready`, `/startup`) so an app can own those
  paths entirely (or expose none). Enabled by default, so behavior is
  byte-identical to before when unset. Completes the probe-conflict work begun
  in #1977 (which already lets a hand-written user route at a probe path win),
  with explicit regression tests for both the user-route-wins and
  probes-disabled cases (#1971).

- **sim-testing:** `#[sim_test]` macro and public `Sim` skeleton for
  deterministic simulation tests (seed-driven, replay-on-panic) (#1797). [no-plugin]

- **security:** `autumn routes audit` (#1604) is now wired into every
  scaffolded app's CI by default — `autumn new` adds a "Route auth coverage
  (security manifest)" step to `.github/workflows/ci.yml`, right after the
  a11y-verify step whose prebuilt CLI it reuses, so a route someone forgot to
  classify fails CI on day one instead of waiting for an app to opt in.
  Unclassified-route diagnostics now also name the offending handler's
  `file:line` (from `file!()`/`line!()`, captured by the `#[get]`/`#[post]`/…,
  `#[ws]`, and static-route macros alongside the existing module path), so a
  failing gate points straight at the line to fix. A new [Route Auth
  Coverage](docs/guide/route-auth-coverage.md) guide documents the
  default-deny posture model and how to classify the three route kinds
  (`gated`, `public`, `framework`), completing the deferred items from #1604's
  first slice (#1850).

### Changed

- **deps:** refreshed the whole dependency floor for the release rather than
  tagging on a month-old lockfile — `diesel` `2.3.10` -> `2.3.12` (upstream's
  Postgres (de)serialization panic fixes and the SQLite batch-insert `.load()`
  fix), `libsqlite3-sys` `0.37` -> `0.38` (bundled SQLite 3.51.x), `redis`
  `1.4` -> `1.6`, plus every semver-compatible update `cargo update` resolves.
  The `time` upper bound widens to `<0.3.56` in the three manifests that carry
  it for the testcontainers/bollard and aws-smithy graphs. This is **not** a
  breaking change for application code: `libsqlite3-sys` is a build-time
  transitive of the optional `sqlite` feature and is not re-exported, so it is
  visible only to an app that already depends on it directly — see the
  [migration guide](docs/migrations/0.7.0.md#upstream-dependency-updates).
  Dependency majors that need API work (`rand` 0.10, `sha1` 0.11, `aes-gcm`
  0.11, `matchit` 0.9, `x509-parser` 0.18, `tokio-tungstenite` 0.30,
  `tokio-postgres-rustls` 0.14) are deliberately not carried here; each is its
  own change.

- **`Query<T>` treats `[` and `]` in a query key as structure (#1972).** A
  target that types a parameter as a plain value — `Query<HashMap<String,
  String>>` is the common one — used to receive `?filter[a]=1` as the literal
  key `"filter[a]"`; it now sees a nested object and reports a decode error
  naming the fix. Give such a field a nested type, or accept it as
  `serde_json::Value`. A key the grammar cannot resolve (one name used as both a
  scalar and a container, or nesting past the cap) fails only if the target
  actually claims it, so an unrecognised parameter stays ignorable exactly as
  before.
- **`Query<T>` rejects a duplicated key in a single-valued position (#1972).**
  `?q=a&q=b` against a `String` field is a 400, as it was under
  `serde_urlencoded` + serde's derive (`duplicate field`). A **map**-typed
  target previously resolved this silently to the last value; resolving
  parameter pollution quietly is how the resulting bugs are built, so it is now
  loud. A sequence field still takes every occurrence.
- **Query decode errors no longer echo the submitted value (#1972).** A
  coercion failure names the field path and the expected type
  (`page: invalid u32 value`) but never the text — the message is returned in
  the 400 Problem Details body and recorded by every error reporter, and a query
  parameter can hold a secret. Key text embedded in an error is bounded and
  stripped of control characters.
- **`Query<Vec<(String, String)>>` yields key-sorted pairs (#1972)** rather than
  submission order; occurrences of one key keep their relative order. The
  `#[edge]` extractor is unaffected — `autumn_edge`'s prelude re-exports axum's
  `Query`.

- **docs:** rewrote the getting-started guide (`docs/guide/getting-started.md`)
  against the current scaffold and CLI. The guide had drifted: it announced the
  "0.4 release line" while pinning 0.6 commands, taught Tailwind v3 directives
  (`@tailwind base;`) against a scaffold that ships v4 (`@import "tailwindcss";`),
  showed a legacy `{"error": {...}}` body where the framework answers RFC 9457
  Problem Details, listed a project tree and an `autumn doctor` transcript that
  no longer matched what `autumn new` and `doctor` actually emit, put
  `AUTUMN_PROFILE` ahead of `AUTUMN_ENV` in the profile precedence chain, and
  pinned `diesel-async` 0.8 against the workspace's 0.9. The prerequisites were
  wrong in a way that broke the first build: the guide said Postgres was needed
  "only if you want database features", but `db` is a default feature, so a
  fresh project links `libpq` at compile time whether or not a database is
  configured — that, the Diesel CLI requirement, and a Docker one-liner for a
  throwaway Postgres are now stated up front. Model examples moved to the
  `BIGSERIAL`/`i64` primary-key convention the generators and `#[repository]`
  assume, replacing `SERIAL`/`i32`.

  Three sections the guide never had were added. First, `autumn generate
  scaffold`, previously reachable only from the generators guide — and
  documented for what it actually produces: the read paths serve immediately,
  while every write stays locked behind both `#[secured]` and the generated
  policy until `autumn generate auth` is run, and the JSON write handlers are
  generated but left unregistered. The guide explains that double gate rather
  than treating it as a caveat, and walks the signup → email-confirmation →
  login flow that unlocks the write views. Second, a testing section built on
  the `tests/integration_test.rs` the scaffold already writes, plus `autumn
  test`. Third, the prebuilt-binary install path. In exchange, the ~100-line
  route-collision-diagnostics appendix and the
  `_method` override's same-origin bullet list were compressed to their
  first-hour essentials and now link onward, and the production checklist moved
  from the middle of the walkthrough to the end. The `#configuration`,
  `#environment-variable-overrides`, and `#route-collision-diagnostics` anchors
  are preserved, so the inbound links from `deployment.md`, `openapi.md`, and
  `skills/autumn-web/SKILL.md` still resolve. Version pins remain on the
  published 0.6.0 line enforced by `first_run_docs_match_current_release_line`.
  [no-plugin]

- **Router: the whole ingress stack is now applied in ONE `Router::layer`
  call.** #2193 collapsed ~26 sequential applications down to a handful; #2198
  collapses what was left — the inner group, the user layers, the middle group,
  the session, and the outer group — into a single
  `router.layer((outer, session, NormalizeBody, middle, user, inner))`. Two
  things blocked that. The session layer's type varies with the configured
  backend, so it could never be a fixed member of a `tower-layer` tuple:
  `build_session_layer` (renamed from `apply_session_layer`, which built *and*
  applied it) now monomorphizes all three backends to
  `SessionLayer<ArcSessionStore>` through the same bridge the custom-store path
  has always used. That costs 1–2 boxed futures per request (a store load, plus
  a save or destroy only when the session is dirty) and buys back a whole
  nesting level — and a nesting level is not a one-off cost, since `Route::call`
  deep-clones everything beneath it on *every* request. The other blocker was
  the registered operator layers; see the next entry. `NormalizeBodyLayer` makes
  explicit the response-body conversion each separate `Router::layer` call was
  doing implicitly (`Route::new` maps through `IntoResponse`); its future is
  un-boxed and it adds no nesting level of its own.

  The number of times a request deep-clones the ingress stack falls from **16
  to 13** on the default feature set (17 → 14 under CI's workspace-unified
  features), which `middleware_stack_depth.rs` gates. Measured end-to-end with
  `valgrind --tool=dhat` against the committed `request_pipeline` bench, with
  the same marginal (zero-iteration-baseline-subtracted) methodology as #2193:
  **~331 → ~222 allocations per request**, together with the `config_arc` work
  above. No middleware changed position; #2193's ordering net passes unchanged.

  [no-plugin] — internal router assembly: no new app-facing API, config key, or
  pattern for the Claude plugin's skills to describe.

- **Registering an app-wide layer no longer deepens the per-request clone
  cascade.** `AppBuilder::layer` and `AppBuilder::static_gate` registrations —
  including the ones plugins make through the same builder — are type-erased at
  registration time and folded into a single composed application, so an
  operator's *n*th layer costs the framework exactly what its first did: **+0**
  ingress traversals per registration, where each one previously added +1
  (#2198). What remains per registration is deliberately shallow and linear —
  one erased box that is cloned (not cascaded) per traversal and one boxed
  response future per request — instead of a `Route` nesting level that both
  re-boxed the future AND joined the quadratic deep-clone cascade. Registration
  order is untouched (first registered is outermost, matching
  `tower::ServiceBuilder`), and the `TypeId`/`type_name` behind
  `has_layer` / `get_layer_types` ride along on the registration, so plugin
  pre-flight checks and the idempotency fail-closed classification behave
  exactly as before. **Breaking:** `IntoAppLayer`'s sealed blanket impl is now
  bound on `tower::Layer` over the new public
  [`app::ErasedAppService`] alias instead of over `axum::routing::Route`. Any
  layer that is generic over the service it wraps — every layer in this repo,
  in the plugins, and in the docs, and every standard tower layer — satisfies
  both bounds, so the move is invisible in practice; only a layer written
  against `axum::routing::Route` specifically stops compiling, and its fix is
  one line (see
  [the migration guide](docs/migrations/0.7.0.md#app-wide-layers-are-now-erased)).
  `docs/guide/middleware.md` describes the new shape.

  [no-plugin] — internal router assembly: the registration API, its ordering
  contract, and the introspection helpers are all unchanged for callers.

- **Router: ~59% fewer heap allocations per request.** The framework's ingress
  middleware stack is now assembled with a handful of *composed* `Router::layer`
  calls instead of ~26 sequential ones (#2193). Every `Router::layer` call wraps
  the whole downstream stack in another `tower::util::BoxCloneSyncService`
  (axum's `Route::layer` ends in `Route::new`), and axum's `Route::call`
  deep-clones that box on every invocation — so *N* stacked layers cost
  `N(N+1)/2 + 2N` heap allocations per request, **quadratically**. Measured
  against axum 0.8.9 with no-op layers: 38 allocations/request at N = 5, 263 at
  N = 20, 1388 at N = 50; the same layers composed into a *single*
  `Router::layer` call cost a flat 16 at any N.

  Measured end-to-end on the real production router (`valgrind --tool=dhat`
  against the new `request_pipeline` bench — three trivial handlers, no DB, no
  business logic, with a zero-iteration baseline subtracted so the figure is
  marginal rather than amortised over router construction): **800.0 → 331.0
  allocations per request**. The number of times a request deep-clones the
  ingress stack fell from 29 to 16.

  `MaintenanceLayer` and `LoadShedLayer` additionally held their path
  allow-lists by value, so each per-request clone deep-copied two `String`s and
  two `Vec<String>`s; both now share them behind an `Arc`. The `UploadConfig`
  extension is installed with `axum::Extension` rather than
  `axum::middleware::from_fn`, dropping a boxed future and a boxed `Next` per
  request.

  **No middleware changed position.** Layers are composed with `tower-layer`'s
  tuple `Layer` impls, whose **first element is outermost** — the reverse of
  consecutive `Router::layer` calls, where the **last** call is outermost.
  `autumn/tests/integration/middleware_stack_order.rs` pins the ordering
  invariants that were previously documented only in prose (metrics attribution,
  panic-to-500 with `X-Request-Id`, trusted-proxy resolution before
  `ClientAddr`, security headers on short-circuits, disabled layers being
  inert); `autumn/tests/integration/middleware_stack_depth.rs` gates the depth;
  and `autumn/benches/request_pipeline.rs` is the profiler workload.

  [no-plugin] — an internal router-assembly change: no new app-facing API,
  config key, or pattern for the Claude plugin's skills to describe.

- **generate model / generate scaffold:** `lock_version` is now a load-bearing
  column name (#1318) — see the Added entry above for the full behaviour. What
  *changes* for anyone who already declared a column with that name: it becomes
  database-managed (dropped from `New{Model}`, so it can no longer be set on
  create), it disappears from a scaffold's HTML form in favour of a hidden
  field, the model gains a derived `etag()` method, and a scaffold that pairs it
  with `--live`, `--sharded`, a `slug` column, or an `Attachment` column — or
  that declares it as the only column, marks it `unique`, or types it as
  anything but a non-nullable `i32`/`i64` — is now refused rather than
  generated. Generation prints a warning naming the escape hatch (rename the
  column) whenever the name is detected.

  **Breaking:** on an `--api` scaffold over a `lock_version` model,
  `#[lock_version]` puts a *required* `lock_version` on `Update{Model}`, so JSON
  `PUT`/`PATCH` clients must now send the version they read. That is what gives
  the JSON path conflict-checking, but existing clients that omit the field will
  fail deserialization. See the
  [migration guide](docs/migrations/0.7.0.md).

- **generate:** finished the zero-JS file-upload slice (#1236) on the read-back
  side. A scaffold with an `Attachment` column now *shows* what it stored: the
  generated `show` and edit views resolve a signed, time-bounded URL through the
  configured `BlobStore` (`attachment_url`) and render a download link
  (`attachment_link`) instead of the literal word "attachment", degrading to the
  stored file's name when no `[storage]` backend is configured. The edit form
  additionally labels the currently stored file above the file input — a file
  `<input>` can't be repopulated, so without it there was no way to tell an
  empty column from one holding a blob you simply didn't replace — and it
  renders identically on the 422 re-render, which is why `update` now loads the
  current row *before* validating (that also runs the record policy before the
  form is handed back, and the policy denial joins the #1872 blob-cleanup paths
  so a forbidden update no longer orphans the just-uploaded file). The generated
  write-path test grew the two AC4 cases — an upload over
  `security.upload.max_file_size_bytes` is rejected with `413`, and a submit
  with no file leaves the optional column `NULL` — and a new (`#[ignore]`d)
  gate compiles *and runs* a freshly scaffolded project's test binary, so the
  emitted tests are proven to pass rather than only string-matched. Finally, the
  generated handler note dropped a false "~2 MiB CSRF size ceiling" warning that
  pushed authors back onto the JavaScript presign path: `form_for` renders the
  CSRF and submit-token hidden inputs as the form's *first* fields, so both land
  inside `security.csrf.token_scan_bytes` however large the upload is. The note
  now names the real limits (`max_file_size_bytes` → 413,
  `max_request_size_bytes` → global body cap). The index list stays a cheap
  presence marker (`widgets::Column`'s cell closure is synchronous and
  `presigned_url` is async, so there is nowhere to await it).

  Follow-ups from the review of that work, in the same slice: the `show` handler
  now runs the record policy before it renders — the link it emits is a signed
  bearer capability for the bytes, and the blob-serving route validates that
  signature alone, so disclosing one from a handler that never consults
  `can_show` would have made the generated policy's "tighten this if shows
  should be gated" comment untrue (a no-op under the default policy, which
  allows reads); the minted blob key now carries a sanitized extension derived
  from the uploaded filename, so a download opens in the right application
  instead of landing as an extensionless `1785…_5cf1a2be-…`; a failure to sign
  logs a warning instead of silently degrading to a link-less name; and
  generating an `Attachment` column now warns when `autumn.toml` has no
  `[storage]` section, since `backend` defaults to `disabled` and the first
  upload would otherwise answer `500 storage not configured`. The two
  `#[ignore]`d gates that compile — and run — a freshly scaffolded project are
  wired into `generator-conformance.yml`; the consolidated `cli_tests` binary's
  only other CI `--ignored` sweep filters on `offsite`, so without that they
  never executed anywhere. `autumn-field__current` / `autumn-attachment__meta`
  are now real classes in `widgets.css` rather than invented ones, and
  `docs/guide/storage.md` documents the scaffolded no-JS path and frames
  presigned direct upload as the opt-in advanced alternative.

  From the PR review: `attachment_url` refuses to issue a URL at all for content
  a browser would execute as same-origin script. The local backend serves blobs
  from the app's own origin, replaying the content type they were uploaded under
  with no `Content-Disposition`, and the default CSP allows `script-src 'self'`
  — so a stored `text/html` or `image/svg+xml` reached by direct *navigation*
  would run with the visitor's cookies, and an anchor's `download` attribute
  governs clicks on that anchor rather than navigation to the URL. The generated
  `LINKABLE_CONTENT_TYPES` is fail-closed and covers the types a scaffold is
  actually used for (images, PDF, plain text, media, archives, office
  documents); anything else still renders on the page, just without a link. It
  also compares `blob.provider_id` against the configured store before signing,
  so a blob left over from a previous backend degrades to the no-link path
  instead of linking to a nonexistent object — or to unrelated bytes that happen
  to share its key.

  Generator-only; no `autumn-web` API change.

- **jobs:** routed the job runtime's recorded timestamps (enqueued_at/started_at/finished_at, due-at filtering, and the backoff-delay computation) through the injected `ClockSource` seam instead of reading `Utc::now()` directly, so recorded job timestamps are deterministic under the sim harness (production defaults to `SystemClock`, behavior unchanged). The in-memory/sim job path is now fully deterministic; the Postgres durable path still uses server-side SQL `NOW()` (#2111). [no-plugin]

- **admin-plugin:** made the core connection surface backend-agnostic so
  SQLite-backend apps can compile it — flipped every hardcoded
  `diesel_async::AsyncPgConnection` to `autumn_web::RuntimeConnection` across
  `routes.rs`, `tokens.rs`, `traits.rs`, `registry.rs`, and the
  `token_admin_db` test, mirroring `autumn-media-plugin`'s `rooms_db.rs`
  (#2090) and `DbSuppressionStore` (#2100). This is an incremental step toward
  #2108: the token admin surface now compiles clean under both Postgres and
  SQLite, but the `experiments`/`feature_flags` models remain Postgres-only
  pending a separate typed-DSL / `Timestamptz` rewrite (tracked in #2108).
  [no-plugin]

- **cli:** aligned the remaining stale `autumn-web = "0.5.0"` test fixtures in
  the `generate` modules (tauri sidecar, scaffold, pwa) to the current `0.6.0`
  release, matching the sibling generators. The end-user pin is unaffected —
  `autumn new` already emits the current version via `CARGO_PKG_VERSION` (#2040).

- **sim-testing:** the deterministic simulation harness (#1797) gained its
  **SQLite DB lane** — the per-sim database substrate a sim builds its app on.
  `sim::substrate::SqliteSubstrate` (gated on the `sqlite` feature) builds a
  fresh, migrated, **in-process in-memory** SQLite pool, unique per simulation,
  ready to hand to the mounted app. It uses a **named shared-cache** in-memory
  database anchored by a **kept-alive guard connection** so the migrated schema
  survives for every pooled checkout — sidestepping the framework's conservative
  in-memory-migration reject precisely because the guard keeps the database
  alive — and a distinct database name per substrate guarantees two sims never
  share state. It also resolves the **feature-unification hazard**: under
  `--features sqlite` the Postgres advisory-lock scheduler and the durable
  Postgres job queue are compiled out, so the sim exercises the *representative*
  local paths — the `InProcessSchedulerCoordinator` (scheduler) and the local
  `JobAdminMemoryBackend` (jobs). The documented divergence: a green sim proves
  the orchestration/timing/ordering of those local paths, **not** the Postgres
  advisory-lock leasing or durable `LISTEN`/`NOTIFY` + `SKIP LOCKED` queue
  claim/lock semantics (consistent with the RFC §12 scope). The substrate is
  self-contained and additive: it hands its `RuntimeConnection` pool to a
  `TestApp` via `TestApp::with_db(...)`, which is exactly the seam W2's
  `Sim::build(TestApp)` consumes — so W2 wires it into the `Sim` app mount in a
  follow-up. (0.7.0 work — do not merge until v0.6.0 is cut.)
- **sim-testing:** the deterministic simulation harness (#1797) gained its
  **chaos lane** — a seed-driven fault-injection builder. `sim::Chaos`
  (`#[non_exhaustive]`, opt-in via the new `Sim::chaos(...)` setter) turns on
  three reproducible faults: `db_transient_errors(p)` (a probability that a `Db`
  checkout returns a retryable `service_unavailable` error), `job_duplicate_delivery(p)`
  (a probability that a job is delivered — and executed — twice, via an
  enqueue-seam re-enqueue of the same `(name, payload)`, to test idempotency),
  and `clock_skew(dur)` (a deterministic wall-clock offset in `[0, dur]` applied
  through a wrapping `ClockSource`). Every fault decision is drawn from a
  **dedicated seeded entropy stream** (`seed ^ salt`, independent of the
  app-facing `Entropy` source), so the **same seed and configuration replay the
  same fault schedule byte-for-byte**; the schedule is recorded and readable via
  the hidden `Sim::__chaos_events()`. `Sim::build` installs the hooks (DB / job
  interceptors + skew clock) only when the config is active, so a default
  (empty) `Chaos` leaves the build byte-for-byte unchanged. Probabilities are
  clamped to `[0.0, 1.0]`. This is W5.0 (chaos scaffolding + chaos-v1 base); the
  richer per-fault surfaces build additively on it. [no-plugin]
- **sim-testing:** a **seeded LLM stub** (`sim::llm`, W5.b, item 6, #1797) — a
  deterministic fake completion client for exercising an agent's retry/fallback
  paths under the paused virtual clock, with **no network and no real model**.
  `SeededLlm::builder(seed)` (or `::from_entropy`) configures canned
  `canned_response(prompt_match, response)` pairs (a seed-derived deterministic
  fallback answers unmatched prompts), an explicit per-call
  `fault_at(call_index, LlmError)` schedule plus an optional probabilistic
  `fault_probability(p, error)` lane, and a `latency_up_to(max)` window; the
  built `SeededLlm` implements the `LlmClient` trait (`LlmRequest` →
  `LlmResponse`/`LlmError`). Every decision is drawn from a **dedicated seeded
  stream**, so the **same seed replays the identical `(response, fault, latency)`
  sequence byte-for-byte** while a different seed diverges; injected latency is
  only observable after `Sim::advance`, keeping it integrated with virtual time.
  Recorded calls are readable via `SeededLlm::calls()`. Standalone and additive
  — it does not route through the `Chaos` builder and touches no default build.
  This is W5.b; it stacks on W5.0 and is a sibling of W5.a (item 5). [no-plugin]
- **sim-testing:** the chaos lane gained a **deterministic SMTP transport fault
  schedule** (W5.a, item 5, #1797), gated on the `mail` feature. `Chaos::smtp_faults([(7, MailFault::Fail), (8, MailFault::Timeout)])`
  maps a **1-based send index** to a `MailFault` (`#[non_exhaustive]`; `Fail`
  returns a permanent-ish `MailError::RuntimeUnavailable`, `Timeout` a
  timeout-shaped `MailError::Io`/`TimedOut`) — the ratified "send #7 fails, #8
  times out" example — so a test can adversarially exercise a throttled-resume /
  retry path against faults that are **deterministic by construction** (a
  scheduled send draws no entropy and never perturbs the DB/job stream).
  Installed at `Sim::build` as a fault-injecting `MailInterceptor`, each send is
  recorded as a new `ChaosHook::MailSend` event on the same
  `Sim::__chaos_events()` log. Under the paused sim runtime a `Timeout` is a
  timeout-shaped error returned immediately (never a real hang), keeping the
  schedule byte-for-byte reproducible. An optional `Chaos::smtp_transient_errors(p)`
  adds a probabilistic lane (drawn from a dedicated mail sub-stream) for parity
  with `db_transient_errors`; an explicit schedule entry always wins. An empty
  schedule / zero rate installs nothing, so a default `Chaos` is unchanged. This
  is W5.a; it stacks on W5.0 and is a sibling of W5.b (item 6). [no-plugin]

- **cli:** the accessibility (a11y) verify step in the generated-app CI template
  (`autumn new`) is now an **enforcing gate** — its `continue-on-error: true`
  escape hatch has been removed, so an a11y violation now fails the job. The step
  was shipped non-blocking in #2018 only until a pinned autumn release published
  prebuilt CLI binaries; with v0.6.0 now published with prebuilt binaries, the
  step runs against the release binary and blocks. Checklist item #7 of #2040.

- **panic gate:** the request-path panic gate (#1611) now also denies
  `clippy::string_slice` and `clippy::arithmetic_side_effects` in every gated
  module, and the manifest grows to 30 modules (adds `inbound_mail.rs`,
  `nested_form.rs` — which carried the header but had drifted out of the
  manifest — and the new crate-private `time_math` saturating-arithmetic
  helpers). `scripts/check-panic-gate.sh` gains header anchoring, anti-spoof
  checks for module-wide `allow`s, `reason =` hygiene on per-site allows,
  reverse-manifest drift detection, a module-count floor, a CI
  feature-reachability check (with a validated, self-expiring exemption list —
  `middleware/trace_context.rs` is behind `telemetry-otlp`, which the lint runner
  cannot enable without `protoc`, and the gate now says so on every run instead
  of leaving it silently unenforced), and a `--self-test` mode that runs by
  default;
  `scripts/pre-push-check.sh` now runs the gate and the gated-features clippy
  lane. CI lints the `inbound-mail`/`inbound-mailgun`/`inbound-ses`/`storage`
  features and runs the inbound-mail test suites. [no-plugin]
- **panic gate:** hardened `scripts/check-panic-gate.sh` against a set of
  reviewer-confirmed bypasses that had passed both the script and `cargo clippy
  -- -D warnings` while shipping a production panic (#1611). Header validation is
  now structural (the block must open *exactly* `#![cfg_attr(not(test), deny(`
  after comment/whitespace stripping), so a widened `all(not(test), any())`
  predicate or a `not(test)` living only in a comment no longer passes. A new
  tree-wide inner-suppression scan rejects any `#![allow(…)]`/`#![expect(…)]`
  (including the `cfg_attr(…, allow(…))` form) that re-permits a gated lint or a
  blanket group (`restriction`/`all`/`pedantic`/`nursery`) across **every** `*.rs`
  under the scan roots — closing the unmarked-submodule hole — while exempting
  `#[cfg(test)]` scopes; the scan roots now include the sibling framework crates
  (`autumn-admin-plugin`, `autumn-media-plugin`, `autumn-storage-s3`,
  `autumn-cache-redis`). Per-site allows must now carry a **non-empty** reason,
  and the feature-reachability check only counts an *enforcing* CI clippy lane
  (`-p autumn-web` + `-D warnings`, not commented out), so a stubbed lane can no
  longer fake coverage. The `--self-test` suite grows to 34 cases, one per bypass.
  CONTRIBUTING.md documents the enforced-subset scoping (the manifest is an
  incremental subset of the request path, not the whole of it) and the
  `macro_rules!` expansion blind spot the gate cannot see. [no-plugin]
- **inbound_mail:** `compute_mailgun_signature` delegates to
  `security::config::hmac_sha256_hex` (output byte-identical); removed a dead
  re-parse in the SNS certificate DER reader (#1611). [no-plugin]

### Deprecated

- **scheduler:** `scheduler::now_unix_secs` and `scheduler::now_unix_duration`
  read real wall time off the injected-clock seam, so a tick key derived from
  them is not reproducible under a `#[sim_test]`. Use
  `time::clock_unix_secs(state.clock())` / `time::clock_unix_duration(state.clock())`
  instead — which is what the framework's own scheduler already does. Neither
  function has a remaining production caller inside autumn (#1797). See
  [the migration guide](docs/migrations/0.7.0.md).

### Fixed

- **deploy:** a cutover no longer orphans an active maintenance flag. The
  maintenance flag path is now resolved through the new
  `AUTUMN_MAINTENANCE_FLAG_FILE` environment variable (falling back to the
  historical cwd-relative `tmp/autumn-maintenance.json` when unset, so a
  non-deploy-managed app is unaffected), and `autumn deploy` stamps it into every
  slot unit it writes, pointing at the per-app `shared/` directory. A slot unit's
  `WorkingDirectory` is the *release* directory — new on every deploy — so on the
  cwd-relative path a cutover silently un-maintained the host, and the blue and
  green slots could not see each other's flag at all. Both read sites (the boot
  load and the 500 ms poller) go through the same resolver. See
  `docs/guide/maintenance-mode.md` (#1621).

- **security:** `#[secured]`'s roles and scopes are no longer lost when another
  guard wraps the handler body. With `#[secured("admin")]` written above
  `#[authorize]`, `#[secured]` expands first and `#[authorize]` then buries its
  role/scope marker consts one level deeper, inside the generated
  `let __autumn_inner: T = (async move { … }).await;` wrapper — a shape the
  marker walks did not descend. Extraction fell through
  to the policy-check fallback, and the route reported `secured: true` with
  empty `roles`/`scopes`: a *provable* manifest dimension silently understating
  the posture it exists to prove. Both walks now descend the generated wrapper
  through the same helper the `#[authorize]` binding walk uses, so the sibling
  extractors can no longer disagree about depth (#1627).

- **security:** two sibling route-metadata losses found by review of the same
  extraction ladder: (1) `#[secured]` above the route macro with `#[authorize]`
  below it dropped the roles/scopes to `&[]` — the live `#[authorize]`
  attribute short-circuited the marker read, so deleting the `#[secured(...)]`
  line produced zero manifest diff; the marker read now runs first. (2) The
  `#[public]` marker walk could not descend a wrapping guard's generated body,
  so `#[public]` above `#[throttle]` lost `public: true` and false-failed the
  coverage gate as `unclassified`; the walk now uses the same shared
  wrapper-descent helper as the other marker extractors (#1627).

- **`generate admin` over an `#[encrypted]` model:** the generated admin adapter
  did not compile — it bound `&new_row` to `.values(…)` and `&diesel_changeset`
  to `.set(…)`, and diesel implements `Insertable`/`AsChangeset` only for the
  owned value once a column uses `#[diesel(serialize_as = …)]`, as every
  encrypted field does. Both now pass owned records when the model has an
  encrypted column; plaintext models keep the borrowed form byte-for-byte.
  Separately, **every admin edit of such a model failed**: the plugin renders an
  encrypted column's edit control disabled and with no `name` (it is managed
  outside the admin), so the form submits no key for it, while the handler
  deserialized the submitted map into `New{Model}`, where the encrypted `String`
  is required — so `serde_json::from_value` returned a "missing field" error even
  when only a plaintext column was edited. Encrypted columns are now excluded
  from the update entirely (matching what the form can actually submit), and the
  handler back-fills a placeholder purely to satisfy that deserialization. That
  exclusion also closes a plaintext write: the `lock_version` update path emits a
  raw `col.eq(value)` tuple that never builds an `Update{Model}` changeset, so it
  would have bypassed the encrypting wrapper and stored **plaintext** in the
  encrypted column.

- **`#[repository]` with hooks over an `#[encrypted]` model:** a repository
  declared `broadcasts = true` (or with an explicit `hooks = …` type) over a
  model carrying any `#[encrypted]` column failed to compile —
  `the trait bound `&Model: AsChangeset` is not satisfied`. The hooks-aware
  bulk-update path bound a *borrowed* proposed row to `.set(…)`, and diesel
  implements `AsChangeset` only for the owned model once a field uses
  `#[diesel(serialize_as = …)]`, as every encrypted field does. It now passes
  the owned record, matching the single-record hooks paths. Reachable from
  `autumn generate scaffold --live` with an encrypted column.

- **failure capsules:** the resolved client identity now obeys
  `[log] filter_parameters`. `client_addr`/`client_host`/`client_scheme` are
  derived from `Forwarded`, `X-Forwarded-*` and `Host`, and were copied into
  the capsule *after* header redaction ran — so an operator who filtered
  `x-forwarded-host` saw it masked under `headers` and sitting in cleartext one
  key away under `client_host`. Each field is now dropped when any header it
  could have been resolved from is filtered — including `X-Real-IP`, a
  fallback source for the address. Where a filtered source actually supplied a
  value the capsule is additionally **refused** by replay: replay pre-inserts
  the recorded identity whole whenever any field survives, so a suppressed
  host would reach the handler as `None` rather than not at all, and a handler
  that branches on it would report a `mismatch` the guide tells operators to
  read as "the bug is gone".
- **failure capsules:** the capsule format version is now `2`. The new
  `db_roles` field changes what a capsule *means* — a reader that skips it
  rebuilds no database topology and replays a shape the recording never had —
  and `serde` would otherwise let an older reader ignore it silently, which is
  exactly what the version gate exists to prevent. Version 1 never appeared in
  a release, so no capsule anyone holds is affected; a capsule written by an
  unreleased build off `trunk-dev` is refused with the usual
  re-record-the-capsule message.
- **failure capsules:** six fidelity and redaction gaps found in review of
  #1598 (#2202). A capsule whose request body the handler read only *partly* —
  or never got to at all — is now marked incomplete and **refused** by replay
  rather than replayed with a shorter body: the handler would otherwise be
  judged on input the failing request never carried, and the resulting
  `mismatch` is exactly what the guide tells operators means "the bug is gone".
  The client identity is recorded again for capsules written by the real
  server: `App::run` wraps the finished router in an *outer*
  `TrustedProxiesLayer` that resolves before the capture scope exists, so the
  inner instance found the extension already present and skipped recording,
  leaving `client_addr`/`client_host`/`client_scheme` empty on every
  production capsule while the test harness (which has no outer layer) recorded
  them. A second cause sat behind the first: the capture layer passed
  `inner.call(req)` to `CAPSULE_SCOPE.scope` as an *argument*, which Rust
  evaluates before the call, so every inner layer's synchronous `call` — where
  a hand-written Tower middleware does its work — ran before the task-local
  existed and saw no scope. The inner call is now made from inside the scoped
  future, which fixes the class rather than the one layer.
  Replay now rebuilds the database *shape* the recording had even when
  the request issued no wire traffic at all — "this request ran no queries" and
  "this application has no database" were the same `None`, so a handler or
  state initializer that checks `state.pool()` or replica availability before
  querying took a branch production never took. Redaction reaches two things it
  used to miss: the credential *inside* a masked header (the token after
  `Bearer`, what a `Basic` credential decodes to, each value of an auth-param
  list such as SigV4's `Signature=`, each cookie value — the form
  a handler actually extracts and may echo into an error message or a SQL bind,
  where the whole header value never
  matched), and values shorter than four characters, which are now masked where
  they stand as a whole token, so a three-digit CVV quoted back by a failure no
  longer reaches disk while timestamps and identifiers stay readable. Finally,
  `SET LOCAL ...` is no longer treated as framework housekeeping: `Db::checkout`
  issues a plain session-level `SET statement_timeout`, so a transaction-scoped
  setting is application code and belongs on the ordered tape, where changing or
  removing it shows up as a divergence instead of being synthesized away.
- **duplicate Markdown heading anchors:** `markdown::render` now hands out
  document-unique heading `id`s. A page that repeated a heading — and real docs
  repeat "Example", "Usage", and "Notes" constantly — emitted the same `id`
  twice, which is invalid HTML and made every table-of-contents entry for the
  repeated heading jump to the first occurrence. Every heading still keeps the
  slug its own text produces, so anchors already published in URLs keep
  resolving; only *repeats* of an already-claimed slug get a `-1`, `-2`, …
  suffix, the convention GitHub, mdBook, and Hugo share. Because the renderer
  reserves every heading's natural slug before handing out any suffix, a repeat
  can never steal a slug another heading owns by name regardless of the order
  the two appear in: `## Example` / `## Example` / `## Example 1` renders
  `example`, `example-2`, `example-1`, leaving `#example-1` pointing at
  "Example 1". Headings with no alphanumeric characters still emit no `id` at
  all and stay out of the anchor namespace.

- **system-tests:** `SystemTest` and `autumn doctor` no longer report
  "no browser" on a Windows host that has Chrome installed (#1456), for three
  separate reasons that all had to go. (1) The candidate list contained no
  Windows locations at all — only Unix paths, plus a `PATH` scan that looked
  for a bare `chrome` (`Path::is_file` does not apply `PATHEXT`, so it could
  never match `chrome.exe`) — so unless `AUTUMN_CHROMIUM` or
  `PLAYWRIGHT_BROWSERS_PATH` was set, nothing was ever probed. Chrome's real
  install locations under `%ProgramFiles%`, `%ProgramFiles(x86)%` and
  `%LOCALAPPDATA%` are now searched, and the `PATH` scan asks for `.exe`.
  (2) The `--version` probe is no longer executed on Windows: `chrome.exe` is
  a GUI-subsystem binary that writes nothing to the parent console, and
  `--version` is not an early-exit switch there, so running it proceeds into
  real browser startup — which is how the report saw exit code 21 (the
  already-running instance being notified) instead of a version string.
  Handing it a private profile would only have replaced that abort with a
  visible browser window and a `Command::output()` call blocking until every
  child closed its stdout. An existing `.exe` is now accepted on file
  evidence — the issue's own suggested fix — and reported as `version
  unavailable`. The extension check is what keeps a mistyped
  `AUTUMN_CHROMIUM` from shadowing a browser that would have worked. (3) On
  Linux/macOS the probe now runs against its own throwaway `--user-data-dir`,
  so it can never rendezvous with a Chrome holding the real profile, and a
  binary that exits 0 without printing (a launcher shim) is still accepted,
  while a spawn failure or non-zero exit still rejects the candidate. The
  platform-dependent decisions live in pure functions taking the platform as
  an argument, so both branches are unit-tested on every host rather than
  being dead code behind `#[cfg(windows)]` on the Linux CI runner.
- **system-tests:** `autumn doctor`'s browser check and the `SystemTest`
  harness now share one implementation (`autumn_web::browser_detect`, not
  behind the `system-tests` feature) instead of keeping separate copies of the
  candidate list and version probe (#1456). The duplicates had already
  drifted, so the CLI could report no browser on a host where the harness
  found one.
- **system-tests:** assertion polling survives more of the CDP errors a page
  transition produces (#1456). `expect_text` / `expect_url` /
  `expect_attribute` / the implicit htmx-settle wait already retried "cannot
  find context with specified id" when a redirect destroyed the JS execution
  context mid-poll; they now also retry `"Inspected target navigated or
  closed"` — the same navigation race, under a name that never contained the
  word "context" — recognise these in chromiumoxide's untyped `ChromeMessage`
  variant as well as the typed one, and match case-insensitively. The match
  is an explicit phrase list, not a bare `contains("context")` and not "any
  Chrome error is transient": a real failure such as `"No node with given id
  found"` (or a JS `"ReferenceError: context is not defined"`) still aborts
  immediately instead of decaying into a five-second timeout with a worse
  message, and `"Target closed"` / `"Session with given id not found"` are
  deliberately excluded because Chrome emits them for a dead renderer too —
  swallowing those would let a crashed page pass the settle fence. The retry
  loop itself is now one shared function with unit tests covering retry,
  fail-fast, and deadline behaviour, so the fix is no longer provable only by
  a browser test.

- **sim-testing:** the op-driver's embedded proptest runners
  (`Sim::gen_ops_with`, `Sim::run_proptest`, #1797) no longer inherit ambient
  `PROPTEST_FORK` / `PROPTEST_TIMEOUT` env vars from `Config::default()`.
  Neither runner has a `test_name` to give proptest's forking machinery, so
  inheriting fork mode panicked with "Must supply test_name when forking
  enabled" before a single op could be generated; both configs now force fork
  mode and the fork timeout off explicitly.
- **mail:** the prod `deliver_later` durability guard no longer aborts app
  startup for applications that never call `deliver_later`/`deliver_later_eager`
  (#2142). Previously, `install_mailer` hard-failed at boot in `prod` whenever
  no durable `MailDeliveryQueue` was registered and
  `mail.allow_in_process_deliver_later_in_production` was unset — even for
  apps that only ever call `Mailer::send`. The check is now enforced lazily:
  startup logs a warning and continues, and `try_deliver_later`/
  `try_deliver_later_eager` return the new `MailError::NoDurableQueueInProduction`
  the first time deferred delivery is actually attempted without a durable
  backend or explicit ack.
- **ci:** wire the `sim_sqlite_substrate` (W4) integration test into the
  `sqlite-runtime` job's `cargo test --features "sqlite,test-support"`
  invocation (alongside `sim_chaos`) so it actually runs in CI, and fix a
  private/redundant intra-doc link in the `sim::Chaos` rustdoc that broke the
  documentation-build gate (#1797). [no-plugin]
- **migrate:** the two `SQLite` migration entry points — `run_pending_sqlite`
  (up) and `revert_user_migrations_sqlite` (down) — now serialize their whole
  list→plan→apply/revert sequence under a single shared `BEGIN IMMEDIATE` write
  lock (#2065, the deferred follow-up from #2062). Previously the read→plan→apply
  window was unlocked, so two concurrent `autumn migrate` / `autumn schema
  migrate` processes against the same file could each read the same pending (or
  applied) set before either wrote, and the loser then re-ran an already-applied
  `up.sql` (or already-reverted `down.sql`) and reported a **false** migration
  failure. The lock is taken through diesel's `AnsiTransactionManager` so diesel's
  own per-migration transactions nest as savepoints (no "cannot start a
  transaction within a transaction"), and a concurrent migrator now queues on the
  connection's `busy_timeout`, re-reads an already-drained set, and cleanly
  no-ops. There is still **no Postgres advisory lock** on the `SQLite` path —
  `SQLite` has no such primitive; the on-disk write lock is the entire
  serialization mechanism. Single-process migrate/revert behaviour is unchanged.
  [no-plugin]
- Docs and crate metadata: updated repository/homepage URLs, README badges, install scripts, and the CI workflow template from the old `madmax983/autumn` owner to `autumn-foundation/autumn` after the GitHub org transfer. Old links still redirect; this makes the canonical URLs correct.
- **migrate:** startup migration auto-apply is now profile-agnostic (#1903).
  Previously the opt-in was name-gated to `prod`/`production`, so a custom
  profile (`fly`, `staging`, …) with `auto_migrate_in_production = true` silently
  fell through to log-only and skipped its migrations (crashing later on missing
  tables). The decision is now convention-over-configuration: `dev`/`development`
  auto-apply by default; every other profile — prod **and** custom — is opt-in.
  A new profile-agnostic `database.auto_migrate` (`Option<bool>`,
  `AUTUMN_DATABASE__AUTO_MIGRATE`) explicitly overrides on any profile, and the
  existing `auto_migrate_in_production` is retained as a back-compat alias now
  honored on any non-`dev` profile (so an existing custom-profile config finally
  takes effect). All applies still route through the advisory-locked runner, and
  an opt-in profile that is left in report-only mode now logs the profile and the
  key to set. The framework shard-map (`control` queue) migration now follows the
  same `database.auto_migrate` decision as app migrations rather than
  force-applying, so it stays consistent with the profile-agnostic convention (and
  no longer fails fatally on an unreachable control target under a report-only
  decision).
- **cli:** `autumn new <name> --with-i18n` (default fullstack flavor, without
  `--api`) now ships the `i18n/` sidecar into its generated Dockerfile, so the
  runtime image no longer panics at boot when `.i18n_auto()` loads
  `i18n/en.ftl` from disk (#1865). The fullstack `Dockerfile.tmpl` gained the
  same builder- and runtime-stage i18n `COPY` anchors the `--api` template
  already carried, resolved by a new `inject_i18n_dockerfile` helper that
  mirrors the `--api` fix (#1847): the `COPY i18n ./i18n` /
  `COPY --from=builder /app/i18n /app/i18n` lines are injected for
  `--with-i18n` and the anchors stripped otherwise (a non-i18n build context
  has no `i18n/` dir), leaving no stray anchor markers.
- **ci:** the Quickstart Gate now falls back to a local source install
  ("PRE-RELEASE MODE") when the README-pinned `autumn-cli` version is not yet on
  crates.io, so the release window between bumping the README and the crate
  publishing no longer turns the gate structurally red. `check-quickstart.sh`
  probes the crates.io sparse index; when the version is unpublished the install
  phase runs `cargo install --path autumn-cli --locked` and the build phase
  patches the generated app's `[patch.crates-io]` to the in-tree `autumn-web`
  (which transitively resolves `autumn-macros` via its own path dep), relaxing
  the registry-provenance assertion to a path-source check. An indeterminate
  publication probe (network/transport error) fails loudly rather than guessing.
  The normal published-version path stays the default and is behavior-unchanged
  (0.6.0 is live, so this is dormant today). [no-plugin]
- **mail:** make `DbSuppressionStore` backend-agnostic
  (`Pool<RuntimeConnection>`) so the `sqlite` + `mail` feature union compiles,
  unblocking the Coverage CI job (#1614). The Coverage lane's
  `--workspace --all-features` catch-all now excludes the two workspace members
  that OWN a `sqlite` feature — `autumn-web` and `autumn-cli` — instead of the
  earlier per-crate exclusion of victim crates. Under `--all-features`,
  autumn-cli's `sqlite` forwarded to `autumn-web/sqlite` and (via global cargo
  feature unification) flipped the shared autumn-web dependency to the SQLite
  backend for the whole graph, which failed to compile every Pg-assuming crate
  (`autumn-admin-plugin`, `autumn-media-plugin`, the example apps) with E0308.
  Excluding the two feature-owners resolves autumn-web to Postgres in the
  catch-all, so those crates compile again and their earlier `--exclude`s are
  dropped; autumn-cli's `postgres`/`sqlite` backends are mutually exclusive and
  it can never be `--all-features`'d anyway (it keeps its dedicated `-p` test
  lanes). A dedicated `-p autumn-web --features "sqlite,mail"` lane preserves the
  `sqlite` + `mail` union compile in the coverage job. [no-plugin]

- **deploy:** the one-time kamal-proxy reboot-durability upgrade (#2070/#2071) no
  longer stamps a new/removed `deploy.tls.host` onto the still-live OLD release
  during its pre-flip forced re-register (#2074). Because kamal-proxy exposes no
  `ServiceOptions` read-back, autumn now records the TLS/host options each forward
  deploy registered in a host-side `shared/proxy-options` marker (`{tls}\t{host}`,
  written atomically alongside the live-slot marker on both first deploy and
  cutover) and reads it back at deploy-start. On a redeploy the durability
  re-register of the still-live old release preserves ITS recorded options (the
  candidate flip still adopts the new config), so a later-op failure + rollback
  leaves the old release on its own host/TLS instead of behind the changed one.
  An absent marker (a legacy host's first redeploy) proceeds as before and
  self-heals by writing the marker; an unreadable marker fails closed at
  pre-flight (mirroring the #2073 port-change refuse) with two-deploy repair
  guidance. Part of #1607.

- **inbound_mail:** quoted MIME parameter values are no longer Latin-1 mangled
  (`filename="café.pdf"` came back as `cafÃ©.pdf`); the parser scans `char`s
  instead of casting bytes (#1611).
- **jobs:** a large `jobs.tracking.ttl_secs` no longer panics the process — the
  tracking stores clamp the TTL and every expiry stamp instead of hitting
  `TimeDelta::seconds`' out-of-bounds panic and `DateTime + TimeDelta` overflow
  (#1611).
- **jobs:** retry on the in-process backend no longer underflows computing
  exponential backoff for a zero attempt counter; the local backend now matches
  the Redis/Postgres backends' saturating exponent (#1611).
- **jobs:** pathological `#[job(unique_for = ...)]` windows and Redis maintenance
  intervals clamp their deadlines instead of overflowing `Instant + Duration`
  (#1611).

- **sim-testing:** `job::enqueue_in` / `enqueue_at`'s delayed enqueues now
  resolve their absolute due instant from the running job runtime's **injected**
  clock instead of `chrono::Utc::now()` (#1797). This was a silent hole rather
  than a cosmetic one: the runtime filters due-at against the injected clock, so
  under a `#[sim_test]` a job asked to run one virtual minute out was stamped due
  years past the sim epoch and **never became due at all** — no amount of
  `Sim::advance` would run it.
- **sim-testing:** `sim::Chaos::clock_skew` no longer leaks real wall-clock time
  into elapsed-time measurements (#1797). Clock skew models a machine whose
  calendar disagrees with reality, which a real monotonic clock is immune to, so
  the skew wrapper forwards `ClockSource::monotonic` to the wrapped virtual clock
  unskewed. Previously it inherited the trait default and read the real machine
  clock.
- **jobs:** the redis worker's maintenance throttles (retry promotion, stale
  recovery, blocked promotion) now hold their deadlines on `tokio::time::Instant`
  rather than `std::time::Instant` (#1797). The loop is woken by
  `tokio::time::sleep`, so the throttle and its counterparty now share one
  timeline — and both are virtualized by a paused runtime for free.
- **jobs:** the admin dashboard's sort key no longer calls `Utc::now()` for a
  record carrying no timestamps at all; it uses `DateTime::MAX_UTC`, which
  expresses the same "sorts newest" intent without making a list's ordering
  depend on when it was rendered (#1797).
- **repo:** `.gitignore`'s blanket `*.sh` silently excluded newly-added
  `scripts/*.sh` from commits, so a CI step invoking one would fail with exit 127
  on a fresh clone while passing locally. `scripts/`, `scripts/lib/`, and
  `scripts/self-hosted-runner/` are now negated back in.

### Security

- **inbound_mail:** cap `multipart/*` nesting at 16 levels (`MAX_MIME_DEPTH`). A
  deeply nested MIME body on an unauthenticated inbound-mail webhook could
  previously recurse until the stack overflowed, aborting the process. Past the
  cap the remaining subtree is kept verbatim as an opaque attachment — never
  dropped (#1611).
- **inbound_mail:** reject MIME boundaries RFC 2046 §5.1.1 does not permit —
  empty, or longer than 70 characters. Such a `Content-Type` now takes the
  existing single-part fallback instead of driving a boundary scan (#1611).

### Performance

- **ingress middleware no longer boxes a future per layer per request:** every
  `axum::middleware::from_fn` on the framework's always-on request path is now a
  hand-rolled `tower::Service` with a **named** future. `from_fn` cannot avoid
  the cost it was paying: the async block it generates has no nameable type, so
  `FromFn::call` returns it as `Box::pin(..)` — one heap allocation per call
  site per request, sized by everything that block captures across its single
  `.await`, which for an outer layer is the whole downstream continuation. DHAT
  measured those boxes at **19.57% of every byte** the `request_pipeline`
  benchmark allocated (5,267,600 of 26,918,238 bytes over 650 requests) while
  being only 2.14% of the blocks — the largest single allocation cost in the
  profile (issue #2214).

  Converted: the asset cache-control layer, the event-bus app context, the
  webhook replay-key cleanup, the method-override rejection filter, the
  trusted-host gate, the startup barrier, the per-request timeout, the
  read-your-own-writes pin, and (under `oauth2`) the HTTP-interceptor scope.
  Each keeps its behaviour and its position in the stack exactly; what goes away
  is the box and, with it, the `self.inner.clone()` `from_fn` needed to move the
  inner service into that box — which for an erased `BoxCloneSyncService` was a
  recursive `clone_box` down the rest of the stack, so each conversion took a
  whole deep-clone cascade with it too.

  Re-run of the issue's own DHAT recipe on `benches/request_pipeline.rs`
  (release, 200 iterations = 650 requests), before and after on the same
  machine — filtering allocation sites whose second stack frame is
  `FromFn<..>::call` exactly, as the issue specifies:

  | | `FromFn::call` `Box::pin` | share of run bytes | marginal blocks/req | marginal bytes/req |
  | --- | ---: | ---: | ---: | ---: |
  | before | 3,250 blocks / 5,215,600 bytes | 19.80% | 168.8 | 36,826 |
  | after | **0 / 0** (0 sites) | **0%** | 139.2 | 25,943 |

  The before column reproduces the issue's measurement exactly on block count
  (3,250) and to within 1% on bytes. Overall: **−17.5% allocations** and
  **−29.6% bytes** per request.

  The same movement is pinned as a regression gate in the debug profile, where
  it is deterministic run to run: **172 → 140 allocation blocks** and
  **37,819 → 26,030 bytes** per request under the default feature set. The ingress clone-on-call traversal count drops from 13 to 9 in
  the same move, on the default set and on a 13-feature build alike. One layer
  also sheds an allocation of its own: the asset cache-control layer no longer
  clones the request path into a `String` on every request in the app.

  Two middlewares are deliberately **not** converted, because they `.await`
  before calling the inner service and their futures therefore cannot be named
  without `type_alias_impl_trait`: the tenancy middleware (async tenant
  extraction) and the rate-limit principal shim (async session read). Both are
  off by default. `webhook_replay_cleanup` keeps one box, taken only on a `5xx`
  that actually registered replay keys; on every other request its future is
  unboxed (it still mints the per-request replay cell it always did).

  One public behaviour change: `read_your_writes::middleware` used to
  `unreachable!()` when handed `ReadYourWrites::Off`. It now debug-asserts and
  falls back to an inert `Off` pin instead of panicking on the request path.
  Both call sites gate on `mode != Off`, so the arm stays unreachable in
  practice. Otherwise nothing public moved: `asset_cache_control`,
  `method_override_rejection_filter` and `webhook_replay_cleanup_middleware`
  remain exported `async fn`s with identical behaviour, now sharing their
  decision logic with the layers so the two forms cannot drift.

  Four gates keep the win from eroding: a per-request allocation **blocks**
  ceiling tight enough that restoring a single `from_fn` fails it, a companion
  **bytes** ceiling derived under the wider feature set CI actually gates with,
  the ingress traversal count pinned to its exact measurement, and a
  `type_name`-based assertion that none of the converted services ever returns a
  `Pin<Box<dyn Future>>` again.

- **config reads on the request path:** generated auth handlers, the admin
  plugin, the `saas` starter, and the `blog`/`saas`/`teams` examples now read
  configuration through `AppState::config_arc()` instead of
  `AppState::config()`. `config()` returns an owned `AutumnConfig`, so every
  call deep-clones **every** config section — 64 allocations and 1,384 bytes
  against a default config, and more as an app's config grows — even to read a
  single `bool` or `usize`. On a handler that cost is paid per request, and a
  handler reading two or three sections paid it two or three times over: a
  downstream app profiling its request path measured whole-config clones at
  ~30% of its per-request allocations and ~42% of its per-request bytes.
  `config_arc()` hands back the shared `Arc<AutumnConfig>` the state already
  holds, so the same read is a refcount bump and handlers borrow the section
  they need off the handle (`&config.auth.password`).

  Nothing about the framework's own ingress path changed — that was already
  allocation-free as of #2199 — and no public signature moved: `config()`
  remains the per-boot owned-snapshot accessor, and the one generated call site
  that still uses it is the boot-time `remember_me_startup` hook, which needs an
  owned `RememberConfig`. Apps calling `state.config()` in their own handlers
  keep compiling; switching them to `state.config_arc()` is the fix, and
  `docs/guide/authentication.md` now teaches that as the default. A new
  generator test pins the emitted handlers to `config_arc()` so the deep clone
  cannot reappear in scaffolded apps.

- **jobs:** the Postgres job worker's claim query (`SELECT … FOR UPDATE SKIP
  LOCKED`) no longer scans and sorts the entire ready backlog for a queue
  before picking one row, for apps that don't configure `[jobs] queues`
  priority (the common case: a single `"default"` queue). The claim query's
  `ORDER BY array_position($2::text[], candidate.queue), candidate.run_at`
  was opaque to the planner — `array_position` depends on the bound queue-
  order array, so even though it's constant across every candidate row when
  only one queue is in play, Postgres couldn't prove that and fell back to a
  `Bitmap Heap Scan` of the whole ready-in-queue backlog followed by a
  `Sort` and `LockRows` over every one of those rows, before `LIMIT 1`
  picked the winner. Single-queue workers now send a query that drops
  `array_position` from `ORDER BY` and uses `queue = $2` (scalar), which
  lets the planner recognize the existing `idx_autumn_jobs_queue_ready
  (queue, run_at)` index order and do a plain `Index Scan` + `Limit 1`
  instead. Measured (`EXPLAIN (ANALYZE, BUFFERS)`, production-shaped
  fixture): 703→21 buffers at 4.4k ready rows, 3,342→22 at 44.6k, and
  57,093→22 (eliminating an external-merge sort spill to disk) at 444k;
  workload-level (`pg_stat_statements`, 50 claims) 166,437→1,410 total
  buffers, a 99.15% reduction. No index or migration changes — see
  `docs/reports/2026-08-14-ledger-job-claim-single-queue/`.

- **state:** `AppState::profile` and `AppState::auth_session_key` no longer
  deep-clone a `String` on every `AppState::clone()`. `AppState` is cloned
  once per hop of the ingress tower stack (`Route::call` deep-clones the
  boxed service beneath it, per #2193/#2198), so the two fields still held
  as an owned `Option<String>`/`String` — rather than shared behind an
  `Arc` like the rest of the struct — paid a fresh heap allocation on every
  one of those clones instead of once per request. Both now live behind an
  `Arc<str>`; `profile()` and `auth_session_key()` are unchanged (`&str`
  via `Deref`), and `with_profile`/`with_auth_session_key` still take
  `impl Into<String>`. Measured with the debug-profile allocation-counter
  gate already used for #2198's `config_arc` work (`autumn/tests/config_alloc_gate.rs`):
  a `TestClient` request drops from 220 to 172 allocation blocks (-22%),
  identical across repeated runs.
- **mail:** list-mail sends (`Mailer::send` with `list_unsubscribe` set) now
  resolve suppression for the whole recipient batch in one query instead of
  one `SELECT` per recipient. The `SuppressionStore` trait gained a batched
  `is_suppressed_many` method (default implementation loops over
  `is_suppressed`, so existing custom stores keep working unchanged);
  `DbSuppressionStore` overrides it with a single `WHERE list_id = $1 AND
  subscriber = ANY($2)` query, chunked at 50,000 recipients on Postgres (a
  backstop against an unbounded single-statement bind, not a tuning knob for
  ordinary sends: a tighter chunk size can land statements past a planner
  cost crossover where `= ANY(...)` stops using the index and falls back to
  a table scan, then re-pay that scan's fixed cost once per chunk) and at
  `repository::MAX_BIND_PARAMS - 1` on SQLite, which binds `eq_any` as one
  parameter per element instead of Postgres's single array parameter.
  Measured
  (`pg_stat_statements`, production-shaped `mail_unsubscribes` fixture):
  statement count per send drops from N to 1 at every batch size tested
  (200/2,000/20,000 recipients); total buffers 660→604 (200), 6,600→6,004
  (2,000), and 66,000→8,070 (20,000, −87.8%). No index or migration changes
  — see `docs/reports/2026-08-15-ledger-mail-suppression-batch/`.
- **scaffold:** the generated `index` page for a `belongs_to`/`references`
  field with a resolved display column (#1146) no longer scans the *entire*
  referenced table to label the ~20 rows on one page. `autumn-cli`'s
  `render_index_reference_label_loads` reused the create/edit form's
  `{name}_select_options` loader — a full, unfiltered `SELECT id, col FROM
  table ORDER BY id` that genuinely needs every row for a `<select>` — to
  build the index's parent-label map too, so every index page view re-read
  the whole referenced table regardless of page size. It now scopes the
  query to `WHERE id = ANY(...)` the page's own FK values
  (`page_data.content`, already fetched), and the identical fix applies to
  the `--belongs-to` nested list (`children_section_with`). Measured
  (`EXPLAIN (ANALYZE, BUFFERS)` + `pg_stat_statements`, production-shaped
  fixture): rows read at the scan node drop from 500,000→20 (-99.996%) at
  500k parent rows, with total buffers 7,051→83 (-98.8%); 707→61 (-91.4%)
  at 50k rows and 72→54 (-25.0%) at 5k rows. No index or migration changes
  — see `docs/reports/2026-08-16-ledger-scaffold-index-label-scope/`.

## [0.6.0] - 2026-07-18

### Added

- **media:** autumn-media gained a full **Rooms** primitive (#1974) — a
  `/api/media/rooms` signaling API with create/join/leave/roster, a
  per-participant `SessionToken` (uuid v4, constant-time **value-only** verify,
  redacted `Debug`, never serialized into the roster) with an advisory
  `token_expires_at` (default 300s TTL) that is **not currently enforced** — a
  valid token keeps authorizing room lifecycle/roster ops until the participant
  is removed or the process restarts — a WHIP publish URL
  plus per-peer WHEP subscribe URLs, a `RoomService` `AppState` extension, and a
  durable `compose_room_recording` grid-composite `#[job]`
  (`FfmpegRoomCompositeCommand`, `xstack` grid video + `amix` audio, on the
  `media` queue). Isolation
  is fail-closed and keyed by `(namespace, room_id)`; the full-mesh cap is **6**
  participants (fail-fast on `0` or `> 6` from config or the builder — no silent
  clamp). Flat `[media]` config keys `room_max_participants` /
  `room_token_ttl_seconds` / `room_namespace` (env `AUTUMN_MEDIA__ROOM_*`)
  configure it, and the in-memory room store is capped at 10,000 rooms
  (`503 RegistryFull` beyond that). Same PR folds in three deferred hardening
  items: sub-second VOD anchoring, MediaMTX paths-list pagination, and rejecting
  dot-only storage-key segments. Recorded limitations: single-process store, no
  `#[job]` timeout/heartbeat, and no automatic reaping of abandoned
  participants; operators must add a `~^room/.+$` MediaMTX path matcher and allow
  the MediaMTX WebRTC origin (`:8889`) in their `connect-src` CSP (#2030).
- **media/deploy:** `autumn deploy` can now provision **MediaMTX as a host
  systemd unit** when a project opts in with `[media.mediamtx] enabled = true`
  (#1974) — it renders `mediamtx.yml` and the unit, then runs
  `daemon-reload && enable --now && restart`, exactly as it already provisions
  kamal-proxy. Four fail-closed host preflight checks run **before** the host is
  touched (FFmpeg resolves, the MediaMTX binary is executable, the recordings
  directory is writable, and the MediaMTX ports are free) — plus a pure-config
  precheck that the configured MediaMTX listener ports are distinct (five checks
  total via `collect_media_doctor_checks`) — and **abort the deploy** if the host
  cannot serve media. The FFmpeg check is fail-closed only for a **concrete
  literal** `[media.ffmpeg] bin`; an env/interpolation-indirected path (empty or
  `${...}`) is deferred to the service runtime as a non-blocking warning. A
  missing or non-executable MediaMTX binary is one of the
  prerequisites that can block `deploy up`.
  `autumn deploy plan` surfaces the media unit, its provisioning steps, and the
  required `connect-src`/`media-src`/`frame-src` CSP origins. The
  `[media.mediamtx]` / `[media.ffmpeg]` deploy config is read straight from the
  merged `autumn.toml` (base ← profile ← `autumn-<profile>.toml`), so that
  media-subtree read itself never routes through autumn-web's `AutumnConfig`
  schema. This does **not** make `strict_config` deploy-safe, though: `deploy::run`
  still calls the strict `AutumnConfig::load()` (for `[deploy]`) ahead of
  `load_media_host_config`, so with `[server] strict_config = true` **and** a
  top-level `[media]` table in the strict-loaded config, that load hard-fails on
  the unknown `[media]` key — both the app runtime and `deploy plan`/`up` exit
  during config load (treat the two as mutually exclusive today). The whole
  controller is a no-op when disabled — a non-media project is byte-for-byte
  unaffected. See the new
  [media deployment guide](docs/guide/deployment.md#mediamtx-host-provisioning-media)
  (#2051).
- **cli:** the declarative-schema command group (`autumn schema`, tracking
  #1975) reached a usable end-to-end shape. `autumn schema diff` targeting SQLite
  now emits real migrations for the previously-refused ALTER-family changes via
  the standard **table-recreate** procedure (create-new → `INSERT..SELECT` →
  drop → rename → recreate indexes, wrapped in `PRAGMA foreign_keys=OFF`…
  `foreign_key_check`… `ON`), coalesced to one recreate block per table;
  Postgres output is byte-for-byte unchanged (#2035). Two new verbs land:
  `autumn schema migrate` applies pending generated migrations against the
  configured database (Postgres advisory-locked, SQLite unlocked;
  provider-locked against the snapshot dialect), and `autumn schema doctor`
  gives a read-only, `--json`-capable health report over the project scaffold,
  snapshot presence, model-vs-snapshot drift, backend provider-lock,
  snapshot-dialect-vs-DB, and pending migrations — exiting non-zero only on an
  actionable error (#2036). `autumn schema diff --write-migration` now **advances
  the checked-in snapshot at generation time** (so `schema migrate` only applies
  files and never touches the snapshot), keeping snapshot and migrations in
  lock-step and leaving un-generated model drift visible (#2042). And
  `autumn schema pull` introspects a live Postgres database into the same
  snapshot IR the model parser produces (with `--dry-run` and a provider-lock
  guard; a SQLite URL is refused loudly), while `schema doctor` gains a
  `database-schema-drift` check (#2045). See the new
  [declarative schema guide](docs/guide/declarative-schema.md).
- **sqlite:** the `#[repository]` / `#[model]` runtime reached CRUD parity on the
  SQLite backend (#1996). Searchable repositories, version-history
  (`versioned = true`, JSON stored as TEXT), and single-record write-RMW sites
  (now wrapped in `BEGIN IMMEDIATE` via `scoped_immediate_transaction`) all work
  on SQLite, and a non-zero `database.statement_timeout` on a `sqlite` URL is
  now **refused at boot** (`PoolError::UnsupportedBackend`) rather than silently
  ignored, since diesel's async `SqliteConnection` has no interrupt hook (#2034).
  Follow-up correctness fixes moved the statement-timeout guard to the
  pool-provider dispatch boundary (so a custom `with_pool_provider` pool cannot
  bypass it), made both sides of SQLite search fold with `to_ascii_lowercase()`,
  and re-based the `BEGIN IMMEDIATE` write reservation on diesel's
  `AnsiTransactionManager` so nested transactions become savepoints (#2038). The
  **durable commit-hook worker** — previously Postgres-only — now runs on SQLite
  too: queued hooks survive restarts, deliver exactly once (reusing the #1995
  idempotency-key dedup), retry with backoff, dead-letter after max attempts, and
  drain gracefully on shutdown, with a `BEGIN IMMEDIATE` single-writer claim and
  fail-closed tenant isolation (the ambient `CURRENT_TENANT` is embedded in the
  persisted context JSON and re-established before each hook) (#2054).
- **sqlite:** SQLite searchable repositories now use **FTS5** instead of the
  interim ASCII `LIKE` fallback (#1910). Search runs over an external-content
  FTS5 virtual table synced by AFTER INSERT/DELETE/UPDATE triggers, with
  `unicode61` tokenization (full-Unicode case/diacritic folding, so `äpfel`
  matches `Äpfel`) and **`bm25` ranking** whose per-column weights come from
  `#[searchable(weight=…)]` priorities (A=10, B=5, C=2, else 1). A new pure
  `repository::sqlite_fts5_match_query` sanitizer is **fail-closed and
  injection-safe**: it tokenizes on Unicode whitespace and turns every token —
  including every FTS5 operator (`AND`/`OR`/`NOT`/`NEAR`/`col:`/`*`/parens/
  quotes/`-`) — into a literal quoted phrase, and empty/no-token input yields an
  empty page with **no query run** (never an unfiltered scan). FTS5 is a hard
  boot dependency: `libsqlite3-sys` is built bundled with `SQLITE_ENABLE_FTS5`,
  the per-connection setup probes FTS5, and a missing FTS5 **fails loudly at
  boot** with an actionable message rather than silently downgrading. The
  `--searchable` / `#[searchable]` scaffold now generates on both backends (#2047).
- **lifecycle:** the `#[state_machine(lifecycle = …, effects(...))]` path now
  **rejects at compile time** an `effects(...)` edge that is not a real
  transition of the referenced lifecycle enum — previously such an edge compiled
  and silently dropped the effect; it now emits a const-eval `assert!` against
  the enum's `STATE_MACHINE_TRANSITIONS` table and fails to compile with a clear
  message (empty `effects` is byte-identical to before) (#2027). Relatedly,
  `autumn lifecycle diagram` now annotates transition edges that carry a
  correlated effect — Mermaid `Draft --> Published : on_commit: AnnouncePublishJob`
  and the DOT equivalent — while effect-free edges render exactly as before
  (#2029).
- **ci:** cargo-deny supply-chain gating — a checked-in `deny.toml` plus a
  standalone `supply-chain` job (`cargo deny check advisories licenses sources`,
  pinned cargo-deny 0.20.2) now blocks on new advisories, un-allowed licenses,
  and unknown sources, with first-run advisories triaged as documented,
  review-dated `ignore`s and a `CONTRIBUTING.md` triage section (references
  #1600) (#2050).
- **ci:** heavy trusted Linux CI lanes can now offload to an **opt-in
  self-hosted Hetzner ephemeral runner set** (default 6, gated on the repo var
  `AUTUMN_SELF_HOSTED_HEAVY == "1"`) via a reusable `runner-routing.yml`; by
  construction every `pull_request` — fork and same-repo alike — stays on
  `ubuntu-latest`, and unsetting the var reverts all lanes to hosted (#2046).
  The `deploy-real-vps` validation harness (#2019) was hardened across several
  fixes: the hcloud CLI pin was corrected (`1.49.1`→`1.66.0`), the retired
  Hetzner server type replaced (`cx22`→`cpx11`), server-type selection made to
  resolve an orderable type at runtime from a refreshed cheapest-first
  preference list across fallback locations, and the kamal-proxy probe pinned to
  `v0.9.2` with a `set -u` crash fixed (#2039, #2043, #2048, #2049, #2052).
- **ci:** tagged releases now publish a native Windows CLI binary
  (`autumn-x86_64-pc-windows-msvc.zip` + `.sha256`) alongside a `scripts/
  install.ps1` and a README Windows install subsection (part of #2005) (#2033).
  The `pam_systemd` deploy end-to-end test was un-gated (the `deploy-e2e-pam`
  cargo feature removed) so it now runs in the default Docker `--ignored` sweep;
  `docs/guide/deployment.md` was reworded to the standard `--ignored` invocation
  (references #2019) (#2032).
- **a11y:** the typed `a11y::TextField` primitive (issue #1706) can now carry
  the wrapper/validation attributes that raw scaffold form-fields need — a
  `class`, the `aria-invalid`/`aria-describedby` error wiring, and the HTML5
  validation constraints `aria-required`/`minlength`/`maxlength`/`min`/`max`/
  `step` — via new chainable builder methods. Crucially, the compile-time
  "must have an accessible name" guarantee is preserved: the new setters are
  available in both type-states but none of them supplies a label, so a
  `TextField<NoLabel>` still cannot be rendered until `.label(..)` /
  `.aria_label(..)` / `.labelled_by(..)` transitions it to `Labeled` (proven by
  a trybuild compile-fail fixture that sets every new attribute and still fails
  to `.render()`). This unblocks routing raw scaffold form-fields through the
  primitive instead of hand-rolled `html!`.
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
  This model form ships for `destroy`, `delete_all`, `nullify`, and `restrict`
  (grandchild recursion and the `delete_many` bulk path included), driving the
  same runtime cascade as the repository attribute; when both a repository
  `dependent(...)` and a model-side `#[has_many(..., dependent = ...)]` are
  declared for the same delete the repository attribute still wins, but a
  debug-only `tracing::warn!` now surfaces the silently-inert model-side
  declaration (issue #1788), and a `dependent`/`on_delete` on a
  `through = <join_table>` association is a directed compile error rather than a
  mis-targeted cascade (issue #1738). See `docs/guide/repositories.md`.
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
  `{prefix}/{profile}/{timestamp}-{token}/{file}` — the remote run id appends a
  short unique token to the timestamp so same-second backups of one profile from
  different hosts never collide; independent remote retention
  (`keep = N`) prunes older uploaded runs only after a verified upload (never the
  just-uploaded run). New `autumn db offsite list` shows the offsite runs for the
  active profile (printing the full `{timestamp}-{token}` run id), and `autumn db
  restore offsite:<profile>/<run-id|latest>` (or `--offsite`) downloads a run to a
  temp dir and applies the same integrity verification and production `--force`
  guard as a local restore. The selector accepts the full `{timestamp}-{token}`
  run id (exact), a bare `{timestamp}` (works only when it uniquely identifies one
  run — otherwise it errors and lists the candidates), or `latest` (newest
  complete run). Transfers use a
  dependency-light synchronous SigV4 S3 client streamed end-to-end to bound memory
  (a single `PutObject` sends a server-side `x-amz-checksum-sha256`; above 64 MiB —
  S3 caps a single `PutObject` at 5 GiB — the artifact uploads via multipart, hashed
  locally and verified after `CompleteMultipartUpload` via HEAD/GET); pointing the
  offsite bucket at the app's
  own `[storage.s3]` bucket at the same endpoint requires the explicit
  `allow_shared_bucket = true` opt-in (issue #1619).
- **doctor:** new `offsite_backup` check (never prints a credential value — only
  env-var names / booleans). It Passes when no `[backup.offsite]` destination is
  configured, or when a configured destination is complete. It **Fails** on an
  invalid configured destination — `[backup.offsite]` set without
  `backup.offsite.s3.bucket`, or a destination that reuses the app's
  `[storage.s3]` bucket at the same endpoint without
  `backup.offsite.allow_shared_bucket = true`. It **Warns** when the bucket is set
  but the named credential env vars are not ready (unset name, or the variable is
  not exported) (issue #1619).
- **alerts:** a failed `autumn db backup` offsite upload now raises a
  `ScheduledTaskFailure` operator alert (dedup key
  `scheduled_task_failure:db-backup-offsite-upload`, title "Offsite backup upload
  failed") through the outbound-HTTP `[alerts]` channels only (PagerDuty / Slack /
  Discord + signed webhook), so an unattended/cron backup never fails its upload
  silently. Email is intentionally excluded — an email-only `[alerts]` config
  builds no channels here and is not notified. Delivery is best-effort on a
  short-lived runtime and can never change the command's exit code; with no
  outbound-HTTP `[alerts]` channel configured no channels are built, so the
  interactive case (message + non-zero exit) is unchanged (issue #1743).
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
  the reference dropdown as raw Maud markup carrying the same HTML5 attributes and
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
- **doctor:** topology-aware queue-coverage (`jobs_queue_coverage`). Declare the
  fleet's worker tiers under
  `[jobs.fleet] tiers = [["critical"], ["bulk", "default"]]` (each inner array is
  one `worker` tier's `jobs.pin`; an empty array is an unpinned tier that drains
  every queue) and doctor proves coverage topology-wide. When a needed queue —
  the configured `[jobs.queues]` unioned with the `#[job(queue = "…")]`-declared
  set — is drained by no tier anywhere, the check is a hard `Fail`, so a normal
  `autumn doctor` run exits non-zero on that gap (not only `--strict`, which is
  the stricter mode that also escalates warnings). A valid multi-tier subset
  split no longer false-positives. The job-declared set is resolved from
  `[jobs.fleet] manifest = "<path>"` (a `queues = [...]` manifest the app emits)
  or an inline `[jobs.fleet] declared_queues = ["…"]`. Absent `[jobs.fleet]` the
  check stays informational-only, exactly as before, so no existing deployment
  regresses (issue #1756).
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
  delivered to it. Slack/Discord webhook URLs must be absolute `https`
  (validated at config load and by `autumn doctor`); the outbound alert sends
  only validate URL shape and do not run through the SSRF deny-list, so restrict
  alert destinations to trusted operator-configured URLs. `autumn alert test
  [--channel <name>]` fires a synthetic alert through each configured
  outbound-HTTP channel and reports per-channel success/error, and `autumn
  doctor` gained an `alert_transports` check that (in production) flags a
  whitespace-mangled routing key, a non-absolute `pagerduty_url`, or a
  non-absolute-`https` Slack/Discord URL (issue #1630).
- **server:** config-driven in-process TLS termination (part of #1603). A new `[server.tls]` section (`cert_path`, `key_path`, optional `reload_interval_secs` (default 60) and `handshake_timeout_secs` (default 10)) makes the app terminate HTTPS itself on the same host:port instead of needing a sidecar proxy. Off by default and gated behind the off-by-default `tls` cargo feature; startup fails fast on a missing/unreadable file, unparseable or empty PEM, a key that does not match the leaf, or an expired/not-yet-valid leaf or intermediate. Certificates hot-reload by polling the cert/key mtimes, so an ACME/`certbot` renewal is picked up without a restart. `autumn doctor` gains a `tls` check that fails on a missing/invalid/expired certificate and warns when the leaf expires within 30 days.
- **server:** automatic ACME (Let's Encrypt) certificate provisioning and renewal (part of #1608). With the off-by-default `acme` cargo feature, a `[server.tls.acme]` section (`domains`, `contact_email`, optional `directory` [Let's Encrypt staging by default], `cache_dir` [default `config/acme`], `http_challenge_port` [default 80], `renew_before_days` [default 30]) obtains and auto-renews the server's TLS certificate over the HTTP-01 challenge — no static cert on disk and no reverse proxy. Static `cert_path`/`key_path` and ACME are mutually exclusive (exactly one must be configured); the issued cert hot-swaps into the same reloadable resolver the `[server.tls]` listener serves, a `:80` listener answers the challenge and redirects HTTP to HTTPS, and a coordinator-leader-elected loop renews hourly before expiry. Single-host slice for now; wildcards/DNS-01 are out of scope (#1620).
- **security:** one-time submit tokens give forms a server-side, at-most-once guarantee with no client JavaScript (issue #1360). A per-render random token is exposed via the `SubmitToken` extractor (plus `SubmitFormField`) and embedded as a hidden `_submit_token` field; on a mutating POST `SubmitTokenLayer` consumes it against the shared idempotency store — first use runs and caches the handler's 2xx/3xx response, a replayed token short-circuits and returns that first response, and a concurrent duplicate that loses the in-flight lock race gets a `409`. If the response stream errors after the handler already committed a 2xx/3xx, the in-flight lock is held (surfacing a `500`, not recording the token) so a retry still gets a `409`. Enabled by default under `[security.submit_token]` and applied inner to the CSRF layer; the consumed-token store inherits `[idempotency].backend`, where an inherited in-memory backend in production only warns while an explicit `backend = "memory"` in production fails fast.
- **security:** build-time route auth-coverage manifest (part of #1604). Every route now carries an exposure classification — `gated`, `public`, `framework`, or `unclassified` — and a new `autumn routes audit` subcommand lists them and emits a stable-ordered (by path then method) JSON security manifest. It is a CI gate: it exits non-zero when any route is `unclassified` (or omitted from the dump), naming each offender. A new `#[public]` marker attribute mirrors `#[secured]` to declare a handler deliberately unauthenticated.
- **web:** new `autumn_web::a11y` module of typed accessible UI primitives that make the accessible name a compile-time obligation (part of #1706). `Img::new(src, alt)` requires alt text (decorative images opt in via `Img::decorative`); `Button::new(name)`/`Button::icon`, `Link::new(href, text)`/`Link::icon`, and `MenuItem::new(name)` all require an accessible name with no nameless constructor (icon-only forms route the name to `aria-label`); `Link::new_tab()` adds `target="_blank"` with `rel="noopener noreferrer"`. `TextField::new` returns a `TextField<NoLabel>` typestate that does not implement `Render` until `.label()`/`.aria_label()`/`.labelled_by()` transitions it to `TextField<Labeled>`, so an unlabeled field is unrepresentable. Inaccessible forms fail to compile (proven by trybuild fixtures); all primitives are re-exported from the prelude under the `maud` feature.
- **cli:** new `autumn a11y verify [PATH] --format <text|json> --strict` command that statically scans project `.rs` files for raw `maud::html!` markup bypassing the typed `a11y` primitives, emitting WCAG-keyed findings (image-alt 1.1.1; label 1.3.1/3.3.2/4.1.2; button-name 4.1.2; link-name 2.4.4/4.1.2) and exiting non-zero to fail CI (part of #1706). The scan is advisory and best-effort — a token-level heuristic over `html!` bodies, not a parser: it skips anything it cannot statically resolve, so the typed primitives remain the compile-time proof of accessibility and this pass only covers the raw-`html!` escape hatch.
- **web:** allowlisted sort and filter for list views (closes #1126). A new `Infallible` `ListQuery` extractor (with `SortDir`) parses `?sort=<col>`, `?dir=asc|desc`, and `?filter[<col>]=<val>` — it never rejects a request, falling back to the model's default order for an empty/unknown `sort` and to ascending for an invalid `dir`. `#[model]` emits typed per-column allowlist helpers and `#[repository]` emits a tenant- and soft-delete/shard-aware `list()` that routes requested keys through Diesel's typed DSL via `.into_boxed()`, so only real columns reach SQL and an attacker-supplied `?sort=id;DROP TABLE users` falls through to the default ordering. Scaffolded non-live, non-owner-scoped index views wire their `data_table` to this automatically.
- **generate:** `autumn generate scaffold` now emits a default-deny record-level `Policy`/`Scope` for every non-`--api` resource (opt out with `--no-policy`); when an owner column is detected (`user_id` → `author_id` → `owner_id` → first `*_id` referencing `users`) it also authorizes the create/edit/update/delete handlers and scopes the index to the current user's rows. A new standalone `autumn generate policy <Model>` command scaffolds `src/policies/<snake>.rs` and wires `.policy(…)`/`.scope(…)` into `main.rs` for an existing model (issue #1125).
- **generate:** `autumn generate scaffold --searchable title,body` makes the named text fields full-text searchable — it adds `#[searchable]` attributes to the model, `searchable` to the `#[repository]`, a migration adding a `search_vector tsvector GENERATED ALWAYS AS (…) STORED` column plus a GIN index, and a search box on the index wired to `GET /<plural>/search` (an empty `?q` falls back to the plain paginated list). Omitting `--searchable` leaves scaffold output byte-identical; the `--live`/`--live-validation` index variants gate the search box off; naming a non-text/unknown field, a uuid-PK model, or an owner-scoped model fails generation before any files are written (issue #1319).
- **generate:** `autumn generate scaffold post avatar:Attachment` now produces working no-JS file uploads end-to-end — the create/update handlers take an `autumn_web::extract::Multipart` extractor, stream each uploaded file to the `BlobStore` via `field.save_to_blob_store(&*store, key)`, and bind the returned `Blob` from a plain `multipart/form-data` submit. `autumn destroy` also no longer strips the `multipart`/`storage` features when a hand-written route still uses them (part of #1236).
- **cli:** `autumn new <name> --api` scaffolds a JSON-first project — handlers return `Json<…>`, `autumn-web` is pinned `default-features = false` to a lean API feature set (`db`, `cache-moka`, `http-client`, `reporting`, `flash`) that drops the maud/htmx/tailwind view stack, and no `static/` tree, `input.css`, `tailwind.config.js`, vendored assets, or Tailwind CI/README notes are generated (the first `cargo run` serves JSON). `--api` cannot be combined with `--daemon` or `--bundled-pg` but composes with `--with-i18n` and `--with-seed`.
- **cli:** `autumn deploy` — first-class push-button deployment of an Autumn app to your own Linux host over SSH, no Docker (issue #1607). It reads a `[deploy]` config block and splits into four subcommands. `autumn deploy check` runs a preflight (SSH reachability, signing secret, database URL, and pending migrations) and reports pass/fail without touching the server — it does **not** print the plan (#1884); `autumn deploy plan` prints the dry-run systemd unit and ordered rollout plan (#1884); `autumn deploy up` uploads a **prebuilt** release binary — built beforehand by a separate `autumn build --embed` step (→ `target/release/<app>`), failing fast with an actionable error if it is absent (`up` never builds from source) — over an injectable SSH executor into a versioned systemd slot on the host, writes the env file, and starts the app (#1912); subsequent deploys perform a **zero-downtime cutover** via kamal-proxy, running pending migrations against the live database *before* flipping the proxy to the new release slot so a request is never served by an un-migrated build (#1928); and `autumn deploy rollback` — plus automatic rollback when a release fails to come up healthy — restores the previous release slot and secrets in one step (#1937). Deploy runs in the **production** profile by default, overridable through a `[deploy]` profile knob (#1956), and a push-button walkthrough is documented in the deploy guide (#1950).
- **testing:** an end-to-end `autumn deploy` acceptance harness exercises the real ssh/systemd/kamal-proxy release lifecycle against a throwaway container — first deploy, zero-downtime cutover, and rollback end to end (AC-7 of #1607, #1949). [no-plugin]
- **web:** `#[lifecycle]` typed state machines prove your domain lifecycles sound at build time (issue #1675) — annotate an enum with its allowed transitions and the framework generates a typed transition API that makes an illegal state change a compile error, with `autumn lifecycle check` validating the declared graph and `autumn lifecycle diagram` emitting a Mermaid diagram of it (#1916). The `transition_controls` view helper renders a record's currently-legal transitions as accessible no-JS form buttons (part of #1326, #1917). `autumn generate scaffold`'s `:states(...)` marker wires those transitions into the generated scaffold, emitting guarded transition routes and controls (#1935), and a field-level `#[state_machine(lifecycle = SomeEnum)]` can now reference a `#[lifecycle]` enum so the two mechanisms share one source of truth (issue #1911, #1944).
- **cli:** SQLite backend foundation (issue #1614) — Autumn now detects a SQLite `database.url`, maps generator DDL to SQLite column types, and `autumn doctor` understands the backend; features SQLite cannot support are rejected at generate time rather than failing at runtime, and the capability/support matrix is documented in a new backend-support guide (#1918).
- **a11y:** four more typed accessible form primitives join `TextField` (part of #1706) — `TextArea`, `Select` with `SelectOption`, `Checkbox`, and `FileField` each make the accessible name a compile-time obligation the same way, and every primitive gains a `.hx(name, value)` escape hatch for attaching arbitrary `hx-*` (and other) attributes without dropping back to raw `html!` (#1946).
- **generate:** `autumn generate scaffold` now routes every generated form field through the typed `a11y` primitives instead of hand-rolled `html!` (part of #1706) — text inputs through `a11y::TextField` (with a new `TextField::label_class` for the label wrapper) (#1913), constrained `String`/`Text` fields through `a11y::TextArea` (#1953), and the shared `FieldControl` rendering through the typed primitives (#1954); `--live-validation` fields render through the primitives plus `.hx()` for their inline-validation `hx-*` wiring (closes #1951, #1964). Scaffolds are now accessible by construction and pass `autumn a11y verify`.
- **security:** security posture manifest v2 (part of #1627) — the build-time security manifest now wraps its output in a provenance envelope and adds CSRF and security-headers coverage dimensions alongside the route auth classification, so a CI consumer can prove the shipped build's CSRF protection and header policy, not just its route gating (#1879).
- **web:** content-negotiated responses let one handler serve both HTML and JSON (#1881) — a new `Negotiate` extractor/response inspects the request `Accept` header and renders the matching representation, so an endpoint can back both a browser page and an API client without duplicating the handler.
- **generate:** `autumn generate scaffold` gains authorization variants (#1830) and owner-scoped list/search codegen (#1841) — generated resources can emit policy-authorized handler variants and scope their index/search queries to the current user's own rows (#1882).
- **web:** nested `has_many` form binding for atomic master-detail saves (#1915) — `NestedChangesetForm` with `inputs_for`, `seeded`, and `blank` renders and binds a parent plus its child collection from a single submit and persists them in one atomic save, so a form like an order with its line items round-trips without hand-written child wiring.
- **sqlite:** Autumn can now boot and serve against a `sqlite://` database behind the new `sqlite` cargo feature. A `RuntimeConnection` alias decouples the runtime from hard-coded Postgres (#1978), the pool applies `busy_timeout`, WAL journal mode, `synchronous = NORMAL`, and `foreign_keys = ON` pragmas per connection (#1987), and startup migrations run through diesel's `MigrationHarness` on a plain SQLite connection with no advisory lock (#1999). A dedicated CI job builds and tests `--features sqlite` (#2008, closes #1614).
- **sqlite:** the generated `#[repository]`/`#[model]` CRUD now targets SQLite as well as Postgres, threading `RuntimeConnection` and backend-aware `RETURNING` handling through the query builders (#2016). New `maybe_for_update!` and `backend_select!` seams emit backend-specific SQL — `SELECT … FOR UPDATE` degrades to a plain read on SQLite (write-write safety still resting on the optimistic `lock_version` check and the pool `busy_timeout`), and multi-row batch inserts / `ON CONFLICT` upserts fall back to SQLite's per-row form while staying tenant- and `lock_version`-safe (#2021).
- **media:** new `autumn-media` plugin — a crate skeleton and config surface (#1976), a `MediaStorage` service with Local and S3 backends (#1998), an FFmpeg encode module (#2003), a MediaMTX transport client with URL builders (#2007), generic media workflows with an artifact sink, retention, and a `MediaWorkflowDelegate` extension seam (#2013), and an `MediaPlugin::from_arroyo_env` migration shim (#2022).
- **schema:** declarative-schema toolchain. A canonical schema IR ships in the new `autumn-schema-core` crate (#1981), a parser lowers `#[model]` structs into that IR (#2001), and `autumn schema snapshot` writes a checked-in schema snapshot (#2009). `#[model]` now accepts the `managed`, `#[unique]`, and `#[references]` markers (acceptance only, no codegen change) (#2014), and `autumn schema diff [--write-migration]` diffs models against the snapshot behind fail-closed guards with correct up/down ordering (#2020).
- **lifecycle:** `#[state_machine]` transitions can now declare per-edge effects. `on = "method"` runs a synchronous effect inside the transition's transaction (an `Err` rolls the transition back) and `on_commit = <Job>` enqueues a job through the transactional outbox with an auto-derived `{model}:{field}:{record_id}:{from}:{to}` idempotency key (#1995, #2017). Effects can also be attached to a `lifecycle = <Enum>` machine at the binding site via an `effects(...)` clause (#2024).
- **auth:** `InMemoryApiTokenStore` is now seedable for tests and local runs via `with_token(raw, principal)` / `with_scoped_token(...)` / `from_env(var, principal)` (#1982, closes #1970).
- **mcp:** MCP tool `inputSchema`s are generated from real request types via a new `OpenApiSchema` derive (#1983), with serde-rename fidelity, collision-proof tool identity, and a build-time flat-query guard that warns when a nested `Query<T>` field should move to a `Json<T>` body (#1992).
- **cli:** the deploy-managed kamal-proxy can terminate TLS on an always-bound port 443 via opt-in `--host` / `--tls` (#1980, closes #1969). `autumn deploy` now uploads the app's `autumn.toml` so the server runs the intended config (#2000), writes per-release manifests, and commits deploy-state markers atomically after the proxy flip (#2015).
- **dist:** prebuilt, curl-installable `autumn` CLI binaries. A tag-triggered release workflow builds Linux musl and macOS (x86_64 + aarch64), and `scripts/install.sh` provides an OS/arch-detecting, sha256-verified `curl … | sh` installer (#2011, closes #2005). Generated-app CI runs `autumn a11y verify` through the same installer (#2018, closes #1931).

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

- **testing:** CSS-selector HTML assertions on `TestResponse` (#1147)
  - Autumn renders server-side HTML (Maud + htmx), so the in-process test client can now assert on page *structure* by CSS selector instead of brittle substrings. New chainable methods on `TestResponse`: `assert_selector(css)`, `assert_no_selector(css)`, `assert_selector_count(css, n)`, `assert_text(css, expected)`, `assert_text_contains(css, sub)`, and `assert_attr(css, attr, expected)`.
  - Non-asserting accessors for custom assertions: `selector_count(css) -> usize`, `selector_text(css) -> Vec<String>`, and `selector_attr(css, attr) -> Vec<Option<String>>` — each returns matches in document order.
  - Backed by a dependency-free HTML parser and CSS-selector matcher (`tag`, `.class`, `#id`, `[attr]`/`[attr=v]`/`[attr^=v]`/`[attr$=v]`/`[attr*=v]`, compound selectors, selector lists, and descendant/child combinators). Parses fragments literally, so bare `<tr>` htmx swaps are selectable — a spec HTML5 tree builder would foster-parent and drop them.
  - Assertions survive cosmetic template changes (whitespace, attribute order, wrapping markup) that break the equivalent `assert_body_contains` test. Failure messages print the selector, expected-vs-actual value, and a truncated outline of the parsed HTML.
  - Purely additive: no breaking change to existing assertions; no new published dependency. See the `autumn::test` module docs and `docs/guide/testing.md` for a worked example.

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

- **sharding:** `from_shard(db: &ShardedDb) -> Self` constructor on generated
  repositories (#1273)
  - `#[repository]` now emits `from_shard` as the standard way to build a
    repository over a shard while preserving full request instrumentation:
    statement timeout, slow-query threshold, and shard-tagged route metric
    label are all carried from the `ShardedDb` context rather than reset to
    framework defaults.
  - **Breaking:** the previous `with_pool` constructor is **renamed** to
    `with_pool_untracked` to signal at the call site that request
    observability is bypassed. Uses of `with_pool` on generated repositories
    must be updated to `with_pool_untracked` (only the name changes; the
    signature and semantics are identical). See the
    [migration guide](docs/migrations/0.6.0.md).
  - `ShardedDb` gains a `#[doc(hidden)]` `__autumn_repository_seed()` accessor
    exposing the `ShardRepositorySeed` carrier struct used by generated code.

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
- **schema:** `autumn schema pull` gained SQLite database introspection (a
  batched `sqlite_master` + `pragma_*` walk into the shared snapshot IR, gated
  on the `sqlite` backend-flip) alongside sharper id-generation fidelity across
  backends: a new `SerialKind`/`serial` marker distinguishes an owned-sequence
  auto-increment PK (`SERIAL`/`BIGSERIAL`, SQLite `INTEGER PRIMARY KEY
  AUTOINCREMENT`) from a plain manually-assigned integer PK, a Postgres
  `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY` clause now round-trips a pull
  verbatim via a new `identity` field, and a PG15+ `NULLS NOT DISTINCT` unique
  index is retained through its version-safe `pg_get_indexdef` text. The new IR
  fields are `#[serde(default, skip_serializing_if)]` so pre-existing snapshots
  stay byte-identical (#2064).
- **cli:** `autumn migrate` up/down/status now run against a `sqlite://`
  database under `--features sqlite`, routed by `DatabaseBackend::detect` through
  the unlocked in-process diesel harness — no Postgres advisory lock, no `diesel`
  CLI subprocess, no content-checksum table, and no control-plane/shard framework
  migrations (their DDL is Postgres-specific). Postgres and libpq keyword/value
  targets keep the historical advisory-locked path byte-for-byte; the default
  Postgres-only build points a detected SQLite URL at a clear "rebuild with
  `--features sqlite`" seam, and a SQLite `migrate status` emits a clear
  provider-appropriate message instead of a raw `diesel` subprocess error (#2062).
- **config:** new `AppBuilder::config_section(root)` seam lets a plugin declare
  ownership of a top-level config table so it is accepted as an opaque,
  never-descended-into subtree under `server.strict_config = true` (the plugin
  owns its own validation) while every other unknown root still hard-fails —
  fail-closed, only explicitly declared roots are exempt. Threaded through every
  config-loading run mode into the strict unknown-key check; `MediaPlugin`
  declares `.config_section("media")` so a media-enabled app no longer fails boot
  with `unknown key "media"` (#2061).

### Fixed

- **docs:** aligned the `autumn-cli` install pin to the workspace `0.6.0` across
  `getting-started.md`, `docs-smoke.md`, `deployment.md`, `websockets.md`, and
  `tutorial/01-project-setup.md` (fixing `repo_hygiene::first_run_docs_match_current_release_line`),
  and removed an obsolete note in `deployment.md` that claimed 0.6.0 lacks
  `autumn deploy` — trunk-dev's in-prep 0.6.0 does ship it (#2037).
- **docs:** fixed the `ProvideProbeState::mark_startup_complete` doctest in
  `autumn/src/probe.rs` (`crate::db::RuntimeConnection` →
  `autumn_web::db::RuntimeConnection`), which reddened `cargo test --workspace`
  because a doctest's `crate` is the synthetic doctest crate with no `db` module
  (#2057). [no-plugin]

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
- **web:** static-first (SSG/ISR) responses now derive their `Content-Type` from the route's file extension instead of hard-coding `text/html`, so the outer compression layer gzips/brotli-encodes compressible pages (with `Vary`) while binary manifest assets keep their real MIME and are left uncompressed; WOFF/WOFF2 fonts are excluded from compression (fixes #752).
- **repository:** hardened `destroy`-cascade codegen so no `before_delete` hook fires until every reachable `restrict` probe has passed — a grandchild `restrict` pre-scan now runs read-only over all selected child ids before the mutating loop, and the parent `restrict` probe precedes the parent `before_delete` across both the model-declared `has_many` and repository-attribute branches. A mixed soft/hard-delete diamond no longer leaves a dangling FK: a new physically-deleted set (distinct from the all-handled set) keys the revisit-skip, so a soft-deleted row no longer suppresses a later hard-delete of the same row (follow-ups to #1789; issue #1800).
- **generate:** `autumn generate auth User --passkeys` again passes `cargo check` against webauthn-rs 0.5.5 (which dropped `CredentialID`'s `Display`/`ToString`) — the generated code encodes credential IDs via a shared `encode_cred_id` helper (base64url, no padding), adds a `base64 = "0.22"` dependency, uses concrete `axum::response::Redirect` return types, reorders the `WebauthnCredential` fields, and warns when a project pins an older `base64` (issue #1822).
- **generate:** `autumn destroy scaffold` on a non-live scaffold no longer strands files when the project's shared `pub fn layout` was lost or renamed — the generate-only shared-layout preflight is now skipped on the revert path, so destroy recomputes its plan and removes the generated files cleanly instead of hard-failing before deleting anything (fixes #1834).
- **cli:** scaffolded container images now report real git provenance on `/actuator/info` instead of null `git.commit`/`git.branch`/`git.dirty` — the generated `build.rs` prefers `AUTUMN_BUILD_*` env vars (threaded from `docker build --build-arg` through Dockerfile `ARG`/`ENV`) over shelling to git, which failed in-container because `.dockerignore` excludes `/.git`, and treats dirty as three-state so an unknown state is omitted rather than reported as `false` (fixes #1676).
- **router:** an app that hand-writes a route at an auto-mounted probe path (`GET /health`, `/live`, `/ready`, `/startup`) now wins and logs an INFO override instead of aborting at startup with an overlapping-route panic (#1977, issue #1971).
- **macros:** a versioned, tenant-scoped bulk upsert no longer returns a 409 when cross-tenant rows are filtered out (#1979, fixes #1963).
- **cli:** `autumn doctor` now resolves deploy secret and database values under the `[deploy]` profile (#2012), and a drifted deploy live-slot marker self-heals from the live proxy at deploy-start (#2023, fixes #1938).
- **ci:** excluded the `sqlite` backend feature from `--all-features` CI steps so Test and Coverage are no longer red (#1997), and fixed a webauthn schema-keys snapshot drift under `--all-features` (#1984, fixes #1959). [no-plugin]
- **lint:** split long first doc paragraphs to satisfy clippy 1.97 `too_long_first_doc_paragraph` (#2025).
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
- **doctor:** `autumn doctor`'s deploy check now fails on a malformed `previous_secrets` block, matching `autumn deploy check` — the two no longer disagree about whether a deploy config is valid (#1904).
- **cli:** `autumn lifecycle diagram` now module-qualifies node ids so two same-named enums in different modules no longer collide into a single node in the emitted diagram (#1929).
- **doctor:** `autumn doctor` now fails a malformed `[[database.shards]]` entry instead of silently treating it as a no-shards configuration, so a typo in a shard block is surfaced rather than passing the SQLite/single-DB path by accident (#1939).
- **security:** CSRF token extraction from a multipart body now scans only a bounded prefix for the token field and streams the remainder rather than buffering the whole body into memory, closing a memory-exhaustion vector on large uploads; the prefix size is tunable via `security.csrf.token_scan_bytes` (#1887).
- **web:** a multipart part with an empty filename is now treated as ordinary non-file input rather than an uploaded file, so a browser submitting an untouched `<input type=file>` (blank filename) no longer produces a spurious empty blob (fixes #1873, #1878).
- **generate:** scaffolded create/update handlers now delete already-uploaded blobs when the handler returns early (e.g. on a validation failure) instead of orphaning them in the blob store (#1888).
- **config:** the startup schema walk no longer aborts when it hits a `statement_timeout`; strict schema enforcement rolls out warn-first, and a new `strict_config_enforce_all` knob opts into failing on every strict finding rather than warning (#1914).
- **security:** tenant-isolation fixes for the repository layer (#1962) — a cross-tenant `update` targeting a foreign or missing id now returns a 404 rather than a 500, and a bulk upsert silently filters out cross-tenant rows for non-versioned records instead of writing across the tenant boundary.
- **media:** autumn-media encode/compose hardening — the `FfmpegvCommand` run paths now cap child output buffering, draining stdout and stderr concurrently with `wait()` (stderr retained only as a bounded tail, stdout drained-and-discarded) so a chatty or hostile encoder can neither grow an unbounded buffer nor wedge on a full pipe; arg builders thread paths as `OsString` (raw bytes preserved via `as_os_str`, byte-wise concat-list escaping) instead of a lossy `display()` conversion, so non-UTF-8 recording paths reach `Command` intact; and the room grid-composite filtergraph is now audio-aware, mixing only the inputs that actually carry an audio stream (and emitting a silent grid with no `amix`/`-c:a` when every input is video-only) rather than failing the whole composite on a lone video-only participant (#2068).
- **deploy:** the standalone `autumn` deploy CLI no longer fail-closes on a plugin-owned top-level config table (e.g. `[media]`) under `server.strict_config = true` — an additive `UnknownRootPolicy::LenientWarn` accepts a genuinely-unknown, table-shaped, true top-level root as opaque with a single doctor-style warning while malformed TOML and unknown keys inside known sections still hard-fail. The CLI structurally cannot know an app's plugin set, so it must not be the strict gate; app boot keeps passing `Strict` and remains the authoritative gate for plugin roots via the `config_section` seam (#2067).
- **deploy:** kamal-proxy routes now survive a host reboot — the proxy state directory is persisted, and on redeploy the shared proxy unit is refreshed and restarted-if-changed (re-registering the still-live release at its actual persisted port) so reboot-durability reaches already-provisioned hosts rather than only freshly-installed ones (#2069, #2071).

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
  a plain TOML string still works. **Breaking:** code that read or set the
  field directly must set it with `Some(value.into())` and read it via
  `secrecy::ExposeSecret::expose_secret()` (supersedes #1304). See the
  [migration guide](docs/migrations/0.6.0.md). [no-plugin]
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
- **generate:** `autumn generate auth`'s browser auth pages now render through the shared 4-arg `crate::layout(title, current_path, flash, content)` — the same helper `autumn new`/scaffold emit — instead of a private bare-DOCTYPE stub; the generator preflights for a shared `pub fn layout` in `src/main.rs` and fails with an actionable message pointing at `autumn new` when it is missing (issue #1353).
- **generate:** scaffolded resources now route every URL through a generated `autumn_web::paths![index, show, new, create, edit, update, delete]` module (plus `events` under `--live` and one `validate_{field}` per validated field under `--live-validation`) instead of hand-written path strings — all view hrefs, form actions, redirects, the SSE `sse-connect`, and inline-validation `hx-post` attributes call `paths::*`, giving each resource's URLs a single source of truth (issue #1133).
- **reliability:** the request-path module set (~25 modules including `form`, `extract`, `idempotency`, session/scheduler/storage/sync, and the middleware stack) is now gated by clippy restriction lints that deny `unwrap`/`expect`/`panic`/`unreachable`/`todo`/`unimplemented`/`indexing_slicing` on non-test builds, and poison-prone locks recover via `unwrap_or_else(PoisonError::into_inner)` rather than panicking — so a poisoned lock or stray unwrap can no longer panic-abort a worker on the request path. A `scripts/check-panic-gate.sh` manifest, run in CI's lint job, keeps the gate from being silently dropped (part of #1611).
- **a11y:** the field DSL now rejects an inline `String`/`Text` length bound greater than `u32::MAX` at parse time (part of #1706, #1913) — an out-of-range `maxlength`/`minlength` constraint that could never be represented in the generated HTML validation attribute is a generate-time error rather than a silently truncated or overflowing value.
- **web:** the htmx form helpers now thread the configured submit-token field name (`[security.submit_token]`) into the hidden field they emit instead of assuming the default `_submit_token`, so a project that renamed the field still gets working one-time-submit protection on helper-rendered forms (#1843, #1883).
- **ci:** CI now discovers Docker-dependent `#[ignore]` database tests dynamically instead of a hard-coded allowlist, so a new testcontainer DB test runs in CI with no workflow edit (#1941). [no-plugin]
- **ci:** added an opt-in real-VPS (Hetzner) deploy-validation workflow with a `pam_systemd` control-socket fix (#2019, closes #1948), and folded ten previously-uncompiled feature-gated `#[ignore]` Docker tests into the CI ignored-sweep (#1985). [no-plugin]
- **a11y:** the accessibility gate now covers the examples and admin-plugin crates, skipping test modules (#1986, part of #1931). [no-plugin]
- **perf:** `paths.rs` percent-encodes directly into a `&mut String`, eliminating per-call heap allocations (#1994).
- **benchmarks:** bumped the puma benchmark dependency to `~> 7.2`, clearing CVE-2026-47736 / CVE-2026-47737 and moderate advisories (#2006). [no-plugin]
- **test:** de-flaked two #1923-sweep tests — a `job_tracking` JSONB cast and a sharding fail-closed replay — surfaced by the Docker sweep (#1968). [no-plugin]

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

## [0.5.0] - 2026-06-16

### Breaking Changes

> Backfilled by #1588. These entries were always in the release — they were
> spelled out inside the feature bullets below rather than called out here,
> which is exactly the archaeology this section exists to remove.

- **security:** forwarded-header trust moved to a single `[security.trusted_proxies]`
  policy and `prod` defaults to trusting **no** proxy until it is configured.
  An app behind a load balancer or reverse proxy that relied on `X-Forwarded-For`
  / `X-Forwarded-Host` being honoured implicitly must declare its proxy boundary
  or it will rate-limit on (and log) the proxy's address. This is the fix for
  #753, #785 and #791. See the [migration guide](docs/migrations/0.5.0.md).
- **security:** `security.rate_limit.trusted_proxies` and
  `security.rate_limit.trust_forwarded_headers` are deprecated in favour of the
  top-level `[security.trusted_proxies]` block. They keep working (registered
  for removal in `1.0.0`) with a startup warning, and `autumn doctor --strict`
  fails when the old and new keys disagree. See the
  [migration guide](docs/migrations/0.5.0.md).

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
- **0.5.0-cleanup:** CSRF multipart fix, reddit-clone feature expansi… (#1250)([12bbb7f](https://github.com/madmax983/autumn/commit/12bbb7f15b4a1b84ad7092efe42581ad1319f6f7))
- Expose recent structured logs via GET /actuator/logfile (#1168) (#1184)([f6dabc0](https://github.com/madmax983/autumn/commit/f6dabc07cc3072dfe321a0dc6828857826c778cb))
- **cli:** Add --api flag for json-only scaffold generation (#1153)([c9b96d1](https://github.com/madmax983/autumn/commit/c9b96d12be11e047ec33dc5e6c2c1f6f6d999028))
- First-class API versioning with deprecation & sunset lifecycles (#1077)([490022b](https://github.com/madmax983/autumn/commit/490022b97991b87d83baf400d5bd8834b50509f5))
- Add transactional test isolation for database tests (#1055)([bb42459](https://github.com/madmax983/autumn/commit/bb42459ba4429d80f1fd10c580478c152f9fe558))
- **cli:** Write autumn.toml stubs, generate OAuth login buttons, and require PKCE verifiers([b922a05](https://github.com/madmax983/autumn/commit/b922a050e9c014f8d7913f5d92e973070a195ee3))
- Implement outbound signed webhooks with retries, DLQ, and actuator endpoints (#792) (#923)([e6e535c](https://github.com/madmax983/autumn/commit/e6e535c5f32010ce9277729a320e6307a9f44df6))
- Postgres full-text search (FTS) with dynamic migrations and repository macros (#842) (#905)([fbc2bf5](https://github.com/madmax983/autumn/commit/fbc2bf50e68a0e5c76071f8e30479dbecf5399fa))
- High-performance bulk repository CRUD operations (issue #841) (#881)([45e39f6](https://github.com/madmax983/autumn/commit/45e39f6ba3e2f2263782ab7d8649ef4b05e482a7))
- **tenancy:** Implement first-class opt-in row-level multi-tenancy (#876)([15029e7](https://github.com/madmax983/autumn/commit/15029e724a620ce9ca04aca585be29f7dbc210b2))
- **db:** Per-query statement timeouts and slow-query telemetry (#826) (#865)([37bfed2](https://github.com/madmax983/autumn/commit/37bfed2b77b2291c2438c92451d43e2e9fb3b12e))
- Expose plugin hooks at framework-owned dependency boundaries (#690) (#862)([ca1b6ce](https://github.com/madmax983/autumn/commit/ca1b6cea0b9b793e3af5ab963fec601638199dfd))

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

### Breaking Changes

> Backfilled by #1588 from `docs/migrations/0.4.0.md`, which shipped with the
> release; the changelog section never named the breaks.

- **security:** `prod` / `production` profiles refuse to bind without a stable
  signing secret (`[security.signing_secret] secret`, or the
  `AUTUMN_SECURITY__SIGNING_SECRET` override). See the
  [migration guide](docs/migrations/0.4.0.md).
- **storage:** the `storage-s3` feature was removed from `autumn-web` and moved
  to the `autumn-storage-s3` crate. See the
  [migration guide](docs/migrations/0.4.0.md).
- **auth:** generated repository APIs require a `policy = ...` in production
  unless `security.allow_unauthorized_repository_api` is set. See the
  [migration guide](docs/migrations/0.4.0.md).
- **mail:** `deliver_later` requires a durable `MailDeliveryQueue` in production
  unless `mail.allow_in_process_deliver_later_in_production` is set. See the
  [migration guide](docs/migrations/0.4.0.md).

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
