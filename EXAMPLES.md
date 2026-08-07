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
| **Key capabilities** | `#[repository]`, `#[scheduled]`, actuator (`/actuator/health`, `/actuator/tasks`), profile layering |
| **Prerequisites** | Rust 1.88.0+, PostgreSQL |
| **Run command** | `cargo run -p bookmarks` |
| **Success proof** | `curl http://localhost:3000/actuator/health` returns `{"status":"UP"}` |

---

### `examples/bookmarks-distributed` — Distributed Deployment

<!-- catalog:example name=bookmarks-distributed tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer deploying an Autumn app at production scale with read replicas |
| **Journey** | Distributed deployment: primary/replica Postgres, Redis-optional, multi-replica web tier behind nginx, one-shot migrator |
| **Key capabilities** | Explicit repository seam, partitioned `#[scheduled]` with advisory locks, `autumn-{profile}.toml` layering, Docker Compose topology |
| **Prerequisites** | Docker and Docker Compose |
| **Run command** | `docker compose -f examples/bookmarks-distributed/docker-compose.yml up -d --build` |
| **Success proof** | `curl http://localhost:3000/api/bookmarks` returns `[]` after the stack is healthy |

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

### `examples/wiki` — Mutation Hooks and Revision History

<!-- catalog:example name=wiki tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer adding audit trails and lifecycle hooks to a content model |
| **Journey** | Hooks/revisions: slug lifecycle, before/after-save hooks, full revision history, REST API |
| **Key capabilities** | `#[model]` hooks, revision tracking, slug generation, generated REST API |
| **Prerequisites** | Rust 1.88.0+, PostgreSQL |
| **Run command** | `cargo run -p wiki` |
| **Success proof** | `curl http://localhost:3000/api/v1/pages` returns `[]` |

---

### `examples/reddit-clone` — Canonical Feature Showcase

<!-- catalog:example name=reddit-clone tier=supported -->

| Field | Value |
|-------|-------|
| **Persona** | Developer building a production-shaped Autumn application and exploring the full feature set |
| **Journey** | Full-stack Reddit clone: registration, sessions, posts, voting, live feeds, background jobs, transactional email, A/B experiments, signed webhook intake, outbound HTTP with SSRF protection, structured error reporting, and live-tunable runtime config |
| **Key capabilities** | `#[secured]`, CSRF, sessions, `#[job]`, `#[ws]` channels, Redis fan-out, `#[scheduled]`, transactional email, htmx voting, `ExperimentService`, `SignedWebhook`, `Client` extractor with SSRF guard, `ErrorReporter`, `RuntimeConfigService` |
| **Prerequisites** | Rust 1.88.0+, PostgreSQL, Redis (optional for local run; required for multi-replica fan-out) |
| **Run command** | `cargo run -p reddit-clone` |
| **Success proof** | `curl http://localhost:3000/` returns the front-page HTML |

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
| Profiles / tasks | `bookmarks` | Repository macro, profile layering, actuator, hourly scheduled task |
| Distributed deployment | `bookmarks-distributed` | Primary + replica Postgres, multi-replica web tier, Docker Compose |
| Horizontal sharding | `bookmarks-sharded` | Tenant → slot → shard routing, control DB, cross-shard fan-out, Docker Compose |
| Hooks / revisions | `wiki` | Before/after-save hooks, slug lifecycle, full revision trail |
| Full-stack showcase | `reddit-clone` | Auth, sessions, jobs, channels, email, A/B experiments, signed webhooks, outbound HTTP, error reporting — the complete feature showcase |
| Multi-tenant SaaS starter | `saas` | Session auth + row-level tenancy + tenant-scoped dashboard — the flagship `autumn new --starter saas` archetype |
| Live mesh rooms | `media-room` | Installs `autumn-media-plugin` with rooms and creates/lists mesh-call rooms through the mounted `RoomService` |
| PDF downloads | `invoice` | Renders one Maud view as both an on-screen page and a downloadable PDF via `autumn_web::pdf::Pdf` |

---

## Release Checklist — Example Drift Gate

Before publishing `autumn-web` or `autumn-cli`, the CI `publish-gate` workflow
runs `scripts/check-examples.sh`. The gate catches:

- Any directory under `examples/` that has no catalog entry (orphan detection).
- Any workspace `examples/*` member that is not cataloged as `supported`.
- Any example listed in `README.md`'s Examples table that is absent from the catalog.
- Any supported example whose `README.md` is missing required quickstart sections.

To add a new example:

1. Create the directory under `examples/` and add a `README.md` with at least
   `## Prerequisites` and `## Quick start` sections.
2. Add it to `[workspace] members` in `Cargo.toml` (if it participates in
   normal validation) or mark it `tier=experimental` or `tier=excluded` here.
3. Add a catalog entry with the machine-readable marker to this file.
4. Add a row to the README.md Examples table.
5. Run `./scripts/check-examples.sh` locally to confirm zero failures.

To retire an example:

1. Either delete the directory or change its tier to `excluded` with a rationale.
2. Remove it from `Cargo.toml` workspace members if it was a member.
3. Remove it from the README.md Examples table.
4. Run `./scripts/check-examples.sh` to confirm zero failures.
