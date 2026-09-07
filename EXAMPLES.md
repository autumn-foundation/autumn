# Autumn Examples Catalog

Every directory under `examples/` is listed here with its support tier,
target persona, demonstrated journey, key capabilities, prerequisites,
exact run command, and the first successful response that proves it works.

The `scripts/check-examples.sh` drift gate reads the machine-readable markers
on each entry and fails a release if the catalog, workspace membership, and
`README.md` Examples table drift out of sync.

Marker format used by the drift gate (HTML comment on its own line inside each entry):

    &lt;!-- catalog:example name=&lt;dir&gt; tier=supported|experimental|excluded --&gt;

---

## Supported Examples

Supported examples participate in normal workspace validation, have a documented
journey, and each carries a README quickstart. A failure in any supported example
blocks publishing `autumn-web` or `autumn-cli`.

---

### `examples/hello` — First Route

<!-- catalog:example name=hello tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer evaluating Autumn for the first time |
| **Journey** | First route: install CLI, run the app, see a response |
| **Key capabilities** | `#[get]`, `routes![]`, `#[autumn_web::main]`, built-in `/health` |
| **Prerequisites** | Rust 1.88.0+ |
| **Run command** | `cargo run -p hello` |
| **Success proof** | `curl http://localhost:3000/hello` returns `Hello, Autumn!` |

---

### `examples/flock` — WASM Island (Yew CSR)

<!-- catalog:example name=flock tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer who needs one heavy, client-side interactive widget inside an otherwise server-rendered Autumn page |
| **Journey** | WASM island: server-render a maud page whose home route mounts a Yew CSR component compiled to `wasm32-unknown-unknown`, with a custom app-level CSP |
| **Key capabilities** | maud-owned page, `data-*` island mount point, ES-module loader, `asset_url` static serving, custom `content_security_policy` with `'wasm-unsafe-eval'` |
| **Prerequisites** | Rust 1.88.0+ (committed wasm artifacts run without a toolchain; rebuilding needs the `wasm32-unknown-unknown` target + `wasm-bindgen-cli`) |
| **Run command** | `cargo run -p flock` |
| **Success proof** | `curl -sD - -o /dev/null http://127.0.0.1:3000/ \| grep -i content-security-policy` shows `script-src 'self' 'wasm-unsafe-eval'`; the browser page animates the flocking canvas |

The island crate that produces the wasm lives in `examples/island-flock`
(cataloged as excluded). See `docs/guide/wasm-islands.md` for the design notes.

---

### `examples/todo-app` — Classic CRUD App

<!-- catalog:example name=todo-app tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer building a full-stack Rust web application with an AI-callable API |
| **Journey** | CRUD app: routes, Diesel model, Maud templates, htmx interactions, bearer-token JSON API, MCP tool projection |
| **Key capabilities** | `#[model]`, Diesel migrations, Maud, htmx, Tailwind, JSON endpoints, `RequireApiToken`, `#[api_doc(mcp)]`, `mount_mcp` |
| **Prerequisites** | Rust 1.88.0+, PostgreSQL |
| **Run command** | `cargo run -p todo-app` |
| **Success proof** | `curl http://localhost:3000/` returns the todo list HTML page; `curl -X POST http://localhost:3000/mcp -H "Authorization: Bearer <token>" -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' ` lists `list_json`, `create_json`, `scan_json` |

---

### `examples/blog` — Admin UI and Static Pre-rendering

<!-- catalog:example name=blog tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer building a content site with an admin interface |
| **Journey** | Admin/static rendering: content CRUD, form validation, pre-rendered public pages |
| **Key capabilities** | `#[static_get]`, `static_routes![]`, `autumn build`, admin UI, input validation |
| **Prerequisites** | Rust 1.88.0+, PostgreSQL |
| **Run command** | `cargo run -p blog` |
| **Success proof** | `curl http://localhost:3000/` returns the blog index page |

---

### `examples/bookmarks` — Profiles, Repository Macro, and Scheduled Tasks

<!-- catalog:example name=bookmarks tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer adding operational features to an existing Autumn app |
| **Journey** | Profiles/tasks: generated CRUD API, actuator endpoints, profile-based config, hourly scheduled task |
| **Key capabilities** | `#[repository]`, `#[scheduled]`, actuator (`/actuator/health`, `/actuator/tasks`), profile layering, app-metrics facade (`autumn_web::metrics` counter + timer on `/actuator/prometheus`) |
| **Prerequisites** | Rust 1.88.0+, PostgreSQL |
| **Run command** | `cargo run -p bookmarks` |
| **Success proof** | `curl http://localhost:3000/actuator/health` returns `{"status":"UP"}` |

---

### `examples/bookmarks-distributed` — Distributed Deployment

<!-- catalog:example name=bookmarks-distributed tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer deploying an Autumn app at production scale with read replicas |
| **Journey** | Distributed deployment: primary/replica Postgres, Redis-optional, multi-replica web tier behind nginx, one-shot migrator, and a two-node cluster the replicas form between themselves |
| **Key capabilities** | Explicit repository seam, partitioned `#[scheduled]` with advisory locks, `autumn-{profile}.toml` layering, Docker Compose topology, self-clustering substrate (`[cluster]`, `ClusterHandle`, cluster-wide counter, `cluster:membership` health) |
| **Prerequisites** | Docker and Docker Compose |
| **Run command** | `docker compose -f examples/bookmarks-distributed/docker-compose.yml up -d --build` |
| **Success proof** | `curl http://localhost:3000/api/bookmarks` returns `[]` after the stack is healthy; `curl http://localhost:3000/cluster` reports a two-member view whose `node` alternates between `web-1` and `web-2` |

---

### `examples/bookmarks-sharded` — Horizontal Sharding

<!-- catalog:example name=bookmarks-sharded tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer scaling tenant data horizontally across multiple Postgres databases |
| **Journey** | Framework-native sharding: tenant id → logical slot → shard, control database for framework state, multi-replica web tier, one-shot multi-target migrator |
| **Key capabilities** | `[[database.shards]]` + `slots` config, `ShardedDb`/`Shards` extractors, concurrent `each_shard` fan-out, `db:shard:*` health components, per-shard metrics |
| **Prerequisites** | Docker and Docker Compose |
| **Run command** | `docker compose -f examples/bookmarks-sharded/docker-compose.yml up -d --build` |
| **Success proof** | `curl -H 'X-Tenant-Id: acme' http://localhost:3000/api/bookmarks` returns `{"shard":"shard0","bookmarks":[]}` |

---

### `examples/wiki` — Mutation Hooks, Revision History, and Markdown Docs

<!-- catalog:example name=wiki tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer adding audit trails and lifecycle hooks to a content model, or serving Markdown-backed docs pages |
| **Journey** | Hooks/revisions: slug lifecycle, before/after-save hooks, full revision history, REST API. Docs: embedded `content/*.md` rendered live at `/docs/{slug}` and pre-rendered to `dist/` by the same handler |
| **Key capabilities** | `#[model]` hooks, revision tracking, slug generation, generated REST API, `markdown` feature (`MarkdownRegistry` + frontmatter + TOC) composed with `#[static_get]` SSG |
| **Prerequisites** | Rust 1.88.0+, PostgreSQL |
| **Run command** | `cargo run -p wiki` |
| **Success proof** | `curl http://localhost:3000/api/v1/pages` returns `[]` |

---

### `examples/reddit-clone` — Canonical Feature Showcase

<!-- catalog:example name=reddit-clone tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer building a production-shaped Autumn application and exploring the full feature set |
| **Journey** | Full-stack Reddit clone: registration, sessions, posts, voting, live feeds, background jobs, transactional email, A/B experiments, signed webhook intake, outbound HTTP with SSRF protection, structured error reporting, cookie consent, and live-tunable runtime config |
| **Key capabilities** | `#[secured]`, CSRF, sessions, `#[job]`, `#[ws]` channels, Redis fan-out, `#[scheduled]`, transactional email, htmx voting (`#[votable]`), threaded polymorphic comments on *two* models (`#[commentable]`, zero comment routes), route-level SEO (`seo(...)` + `SeoMeta`, a DB-backed `SitemapSource`, `/robots.txt` + `/sitemap.xml`), `ExperimentService`, `SignedWebhook`, `Client` extractor with SSRF guard, `ErrorReporter`, `RuntimeConfigService`, typed accessible form primitives (`a11y::TextField`/`TextArea`/`Select`/`Button` — an unlabeled field does not compile), the `ChangesetForm` validation round-trip with inline errors and a no-JavaScript form POST, sanitized user-submitted rich text (`markdown::render_user_content`), offset pagination (`PageRequest` + `pagination_nav`, plain `<a href>` page links), cookie consent (`inject_consent_banner`, the `Consent` gate, a withdraw flow), failure capsules (`[failure_capture]` behind the `capsules` profile + a committed capsule and an `autumn replay` walkthrough), and deterministic simulation testing (a seeded `#[sim_test]` over the hot-rank decay curve) |
| **Prerequisites** | Rust 1.88.0+, PostgreSQL, Redis (optional for local run; required for multi-replica fan-out) |
| **Run command** | `cargo run -p reddit-clone` |
| **Success proof** | `curl http://localhost:3000/` returns the front-page HTML *and* the cookie-consent banner; `curl http://localhost:3000/sitemap.xml` returns a `<urlset>` listing the site's communities and posts; `curl 'http://localhost:3000/r/rust?page=2'` returns page 2 with a `<nav aria-label="Pagination">` of plain links |

---

### `examples/saas` — Multi-Tenant SaaS Starter

<!-- catalog:example name=saas tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer evaluating Autumn who wants a complete, runnable SaaS archetype rather than hand-assembled primitives |
| **Journey** | Multi-tenant SaaS: sign up an organisation → log in → a tenant-scoped dashboard that only ever shows the signed-in organisation's projects |
| **Key capabilities** | Session auth (`Session` + bcrypt `hash_password`/`verify_password`), row-level multi-tenancy (`#[repository(tenant_scoped)]` + `with_tenant`), Maud + htmx UI |
| **Prerequisites** | Rust 1.88.0+, PostgreSQL |
| **Run command** | `cargo run -p saas` |
| **Success proof** | After signing up in the browser, `GET /dashboard` returns `200 OK` with the tenant's projects; a second organisation never sees the first's data |

This is the flagship built-in starter behind `autumn new <name> --starter saas`.
The committed tree here is the rendered form of the embedded starter; the
`embedded_saas_matches_example_saas` test in `autumn-cli` keeps the two in lock-step.

---

### `examples/teams` — Organization Membership & Email Invitations

<!-- catalog:example name=teams tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer building a multi-user B2B SaaS who needs teammates, roles, and email invitations without hand-rolling the join table, token record, and accept route |
| **Journey** | Team membership: sign up → get a personal organization as `Owner` → invite a teammate by email and role → they accept (signup-then-join or direct join) → a role-gated member-management screen |
| **Key capabilities** | Row-level multi-tenancy with the active organization as tenant (`#[repository(tenant_scoped)]`, issue #695), a closed `Role` enum + hierarchy-aware `require_role` guard layered on the existing session/Policy role plumbing (issue #496), `#[mailer]` invitation email, idempotent invite-accept, last-`Owner` protection |
| **Prerequisites** | Rust 1.88.0+, PostgreSQL |
| **Run command** | `cargo run -p teams` |
| **Success proof** | After signing up in the browser, `GET /members` returns `200 OK` showing the signer as `owner`; inviting a teammate and following the accept link in the dev mailbox creates exactly one `Membership` row even on a double-click |

---

### `examples/media-room` — Live Mesh Rooms

<!-- catalog:example name=media-room tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer building interactive video/audio who is evaluating `autumn-media-plugin` |
| **Journey** | Install the media plugin with rooms → create a room from an app handler via the plugin-installed `RoomService` → list the created rooms |
| **Key capabilities** | `MediaPlugin::new().config(media).with_rooms()`, the `RoomService` `AppState` extension, the plugin-mounted room routes under `/api/media`, a shared `AppState` extension |
| **Prerequisites** | Rust 1.88.0+ |
| **Run command** | `cargo run -p media-room` |
| **Success proof** | `curl -X POST http://localhost:3000/rooms` then `curl http://localhost:3000/api/rooms` returns the created room's JSON |

Boots with no database or `MediaMTX` server; the companion narrative is
`docs/guide/media.md`.

---

### `examples/invoice` — PDF Downloads

<!-- catalog:example name=invoice tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer building billing/reporting features who needs a downloadable PDF |
| **Journey** | Render one Maud view as both an on-screen detail page and a downloadable PDF via `autumn_web::pdf::Pdf` |
| **Key capabilities** | `autumn_web::pdf::Pdf::from_markup`, `.filename(...)`, the `Clock` extractor for deterministic rendering, `TestResponse::assert_pdf_contains` |
| **Prerequisites** | Rust 1.88.0+ |
| **Run command** | `cargo run -p invoice` |
| **Success proof** | `curl -OJ http://localhost:3000/invoices/42/pdf` downloads `invoice-42.pdf` |

Boots with no database; the companion narrative is `docs/guide/pdf-downloads.md`.

---

### `examples/react-graphql` — React SPA + GraphQL Plugin

<!-- catalog:example name=react-graphql tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer with a TypeScript/React front end who wants an Autumn backend, and wants to see how a GraphQL surface interacts with `#[model]`, `#[repository]`, hooks, and the pool |
| **Journey** | SPA + GraphQL on a real model: Autumn renders the page shell → a committed Vite/React 19/TypeScript bundle mounts into it → the bundle queries and mutates over GraphQL served by a generic `GraphqlPlugin` → every resolver goes through the generated `PgNoteRepository`, so normalisation, validation, and hooks behave identically for GraphQL and the generated REST handlers |
| **Key capabilities** | `#[model]` with `#[normalize(trim)]` + `#[validate]`, `MutationHooks` (`before_create` validation, `before_delete` rule), `#[repository(hooks = …, api = …)]` with a derived finder and generated REST CRUD mounted beside GraphQL, `PgNoteRepository::with_pool_untracked` from `AppState` in resolvers, an `on_startup` seed that runs once across instances under a transaction-scoped advisory lock, embedded migrations, `AutumnError` → GraphQL error with `extensions.status`, `Plugin` + `AppBuilder::nest` + `declare_plugin_routes` (audit-clean raw router), `PluginContract` + `plugin_conformance::run_conformance`, Maud shell + `asset_url` under the default `script-src 'self'` CSP, `autumn build --embed` single-binary deploy (`embed_static!` + `.embedded_static`), `GET /graphql/sdl` with a committed-SDL drift test, two-tier tests (`TestApp` without Docker; `TestDb` testcontainer with the real migration applied) |
| **Prerequisites** | Rust 1.88.0+, PostgreSQL (`docker compose up -d` in the example directory provides one); the React bundle is committed, so Node 20.19+/22.12+ is needed only to change the frontend |
| **Run command** | `cargo run -p react-graphql` |
| **Success proof** | `curl -s http://127.0.0.1:3000/graphql -H 'content-type: application/json' -d '{"query":"{ notes { title } }"}'` returns `{"data":{"notes":[{"title":"Welcome to Autumn Notes"},{"title":"Try the GraphQL endpoint"}]}}` (seeded on boot); `curl -s http://127.0.0.1:3000/api/notes` returns the same rows through the generated REST handler; the browser page lists them and adds a third from its form without a reload |

---

## Experimental Examples

Experimental examples **are** workspace members — they compile, lint and test
with the rest of the workspace, and a break in one fails CI like any other
crate. What they are exempt from is the supported-fleet contract: the Chromium
e2e fan-out in `scripts/check-examples-e2e.sh`, the README Examples table, and
the Journey Map. An example belongs here when its proof is not "a browser can
load a page" — it has its own dedicated CI job instead.

---

### `examples/edge-greeting` — Edge Capsule (wasm32-wasip1)

<!-- catalog:example name=edge-greeting tier=experimental -->

| Field | Value |
|-------|-------|
| **Persona** | Developer who wants a read-path route served from the CDN edge without maintaining a second codebase |
| **Journey** | Edge lane: mark a `GET` handler `#[edge]`, register it with `edge_routes![]`, and get a portable `wasm32-wasip1` capsule out of the same `autumn build` — with the origin binary still the authority and the fallback |
| **Key capabilities** | `#[edge]`, `#[edge(needs(kv))]`, `edge_routes![]`, `autumn_edge::serve`, `EdgeCache` over the `EdgeKv` seam, `AppBuilder::with_edge_kv`, the NDJSON capsule wire protocol |
| **Prerequisites** | Rust 1.88.0+ and the `wasm32-wasip1` target (`rustup target add wasm32-wasip1`) |
| **Run command** | `cargo run -p edge-greeting` (origin); `cargo build -p edge-greeting --target wasm32-wasip1 --release --bin edge-capsule` (capsule) |
| **Success proof** | `curl http://localhost:3000/greet/ada` returns `Hello, ada!`; `cargo test -p edge-greeting --test conformance -- --ignored --test-threads=1` proves the capsule answers a request corpus byte-identically to the origin |
| **Rationale for the tier** | Its proof is a conformance suite against a real wasm artifact, not a Chromium smoke, and it needs a toolchain target the supported fleet does not install. The dedicated `edge-conformance` CI job runs it on every push. |

Boots with no database; the companion narrative is `docs/guide/edge.md` and the
design record is `docs/adr/0011-edge-capsule-read-lane.md`.

---

### `examples/hot-upgrade` — In-Place Upgrade (`SIGUSR2` socket + state handoff)

<!-- catalog:example name=hot-upgrade tier=experimental -->

| Field | Value |
|-------|-------|
| **Persona** | Operator of a single-binary Autumn app who wants to deploy continuously without a load balancer, a cold cache, or a dropped connection |
| **Journey** | In-place upgrade: designate a block of live state, build a new binary whose state shape changed, `kill -USR2`, and watch the same socket keep serving while the state is carried across a compile-checked migration |
| **Key capabilities** | `AppBuilder::with_live_state`, `with_live_state_from`, `AppState::live_state`, `state_migration!`, `LiveStateHandle` freeze semantics, `[server.upgrade]` |
| **Prerequisites** | Rust 1.88.0+ on Linux or another Unix. No database, no config file |
| **Run command** | `cargo build -p hot-upgrade`, then `AUTUMN_UPGRADE_BINARY=target/debug/hot-upgrade-v2 ./target/debug/hot-upgrade-v1` |
| **Success proof** | `cargo test -p hot-upgrade --test live_upgrade` boots the real v1 binary, upgrades it to the real v2 binary under sustained load, and asserts zero refused connections, zero failed reads, 100% carry-over of the pre-upgrade value, and a bounded cutover latency spike |
| **Rationale for the tier** | Its proof is a live two-process upgrade under load, not a Chromium smoke: what it demonstrates has no page to click. The test runs in the ordinary `cargo test --workspace` lane. |

Two binaries rather than one — the whole point is the *old* build becoming the
*new* one. The companion narrative is `docs/guide/hot-upgrades.md`.

---

## Excluded Examples

Excluded examples are intentionally kept out of the workspace and the normal
adoption path. They are not runnable Autumn servers, do not participate in
workspace compilation or test validation, and never block a release. They exist
to document spikes and specialised build targets.

---

### `examples/island-flock` — Yew WASM Island Spike

<!-- catalog:example name=island-flock tier=excluded -->

| Field | Value |
|-------|-------|
| **Persona** | Framework contributor prototyping client-side interactivity inside an Autumn page |
| **Journey** | WASM island spike: compile a Yew CSR component to `wasm32-unknown-unknown` and mount it as an island in server-rendered HTML |
| **Key capabilities** | Yew CSR component, `cdylib` crate, `wasm32-unknown-unknown` target, hand-rolled island bootstrap |
| **Rationale for exclusion** | This is an exploratory spike, not a supported server example. It is a `cdylib` targeting `wasm32-unknown-unknown`, so it cannot compile as a normal workspace member and is listed under `exclude` in `Cargo.toml`. It has no HTTP server, no README quickstart, and no place in the Journey Map. |
| **Build command** | `examples/island-flock/build-island.sh` |

See `docs/guide/wasm-islands.md` for the design notes behind this spike.

---

## Journey Map

The table below maps each example to a distinct learning journey so evaluators
can pick the closest starting point without overlap.

| Journey | Example | One-line summary |
|---------|---------|-----------------|
| First route | `hello` | Simplest possible Autumn app — three routes, no database |
| WASM island | `flock` | Server-rendered maud page that mounts a Yew CSR "literary boids" wasm widget on `GET /` |
| CRUD + MCP | `todo-app` | Full-stack todo list with Diesel, Maud, htmx, bearer-token API, and MCP tool projection |
| Admin / static rendering | `blog` | Blog engine with admin UI and `#[static_get]` pre-rendering |
| Profiles / tasks | `bookmarks` | Repository macro, profile layering, actuator, hourly scheduled task, app-metrics counter + timer |
| Distributed deployment | `bookmarks-distributed` | Primary + replica Postgres, multi-replica web tier, a two-node self-clustering substrate with no coordination service, Docker Compose |
| Horizontal sharding | `bookmarks-sharded` | Tenant → slot → shard routing, control DB, cross-shard fan-out, Docker Compose |
| Hooks / revisions | `wiki` | Before/after-save hooks, slug lifecycle, full revision trail |
| Markdown docs + SSG | `wiki` | `markdown` feature: embedded `.md` with frontmatter, TOC, heading anchors, rendered live at `/docs/{slug}` and pre-rendered via `#[static_get]` |
| Full-stack showcase | `reddit-clone` | Auth, sessions, jobs, channels, email, A/B experiments, signed webhooks, outbound HTTP, error reporting, route-level SEO, accessible forms, rich text, cookie consent, pagination, failure capsules and a seeded `#[sim_test]` — the complete feature showcase |
| Multi-tenant SaaS starter | `saas` | Session auth + row-level tenancy + tenant-scoped dashboard — the flagship `autumn new --starter saas` archetype |
| Live mesh rooms | `media-room` | Installs `autumn-media-plugin` with rooms and creates/lists mesh-call rooms through the mounted `RoomService` |
| PDF downloads | `invoice` | Renders one Maud view as both an on-screen page and a downloadable PDF via `autumn_web::pdf::Pdf` |
| SPA + GraphQL plugin | `react-graphql` | Autumn-rendered shell, committed Vite/React/TypeScript bundle, and a generic `GraphqlPlugin` whose resolvers go through a `#[model]`/`#[repository]` with hooks — the same rows also served by generated REST |

---

## Release Checklist — Example Drift Gate

Before publishing `autumn-web` or `autumn-cli`, the CI `publish-gate` workflow
runs `scripts/check-examples.sh`. The gate catches:

- Any directory under `examples/` that has no catalog entry (orphan detection).
- Any workspace `examples/*` member that is cataloged as neither `supported`
  nor `experimental` (an `excluded` member, or none at all).
- Any example listed in `README.md`'s Examples table that is absent from the catalog.
- Any supported example whose `README.md` is missing required quickstart sections.

To add a new example:

1. Create the directory under `examples/` and add a `README.md` with at least
   `## Prerequisites` and `## Quick start` sections.
2. Pick a tier. `tier=supported` is the default: a workspace member with a
   README quickstart, a README.md table row, a Journey Map entry, and a
   Chromium smoke in the fleet e2e gate. `tier=experimental` is a workspace
   member that carries its own dedicated proof instead of the fleet smoke (see
   the Experimental section above). `tier=excluded` is not a workspace member
   at all.
3. Add a catalog entry with the machine-readable marker to this file.
4. Add a row to the README.md Examples table (supported tier only).
5. Run `./scripts/check-examples.sh` locally to confirm zero failures.

To retire an example:

1. Either delete the directory or change its tier to `excluded` with a rationale.
2. Remove it from `Cargo.toml` workspace members if it was a member.
3. Remove it from the README.md Examples table.
4. Run `./scripts/check-examples.sh` to confirm zero failures.
