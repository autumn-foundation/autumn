# Autumn + React + GraphQL Example

A **TypeScript React** single-page app on top of an **Autumn** backend, talking
**GraphQL** through a plugin, against a real **Postgres** model. Read it in
this order:

1. **`src/models.rs`, `src/hooks.rs`, `src/repositories.rs`** — the data
   layer, all framework-native: one `#[model]` with `#[normalize(trim)]` and
   `#[validate]`, a `MutationHooks` impl, and a `#[repository]` that also
   generates JSON REST handlers over the same rows.
2. **`src/notes.rs`** — the `async-graphql` resolvers. Each one builds the
   generated `PgNoteRepository` from the pool on `AppState`. GraphQL is one
   more door into the model, not a second data layer: trimming, validation,
   hooks, and transactions are identical whether a write arrives over
   GraphQL or the generated REST handler.
3. **`src/graphql_plugin.rs`** — a generic `GraphqlPlugin` that adapts any
   `async-graphql` schema onto an Autumn app. It is the extensibility
   showcase: a `Plugin` that mounts a raw router, declares its routes for
   `autumn routes` and the audit gate, installs an `AppState` extension, and
   states a `PluginContract`. Nothing in it knows about notes.
4. **`frontend/`** — Vite + React 19 + TypeScript with a 40-line typed
   GraphQL client and no client-side framework beyond React. Its build output
   is committed under `static/app/` and served by Autumn's standard `/static`
   mount, so running the example needs no Node.

Autumn renders the page shell (and owns the security headers); React owns
`#root`. The default `script-src 'self'` Content-Security-Policy is left
untouched because the bundle is an external ES module.

## What it demonstrates

| Feature | Where | What it does |
|---------|-------|--------------|
| `#[model]` with `#[normalize(trim)]` + `#[validate]` | `src/models.rs` | One struct yields `Note`, `NewNote`, `UpdateNote`; input is canonicalised before validation on every write path |
| `MutationHooks` | `src/hooks.rs` | `before_create` runs the model's rules for direct repository callers; `before_delete` refuses to delete a pinned note — inside the transaction, for every door |
| `#[repository(Note, hooks = …, api = "/api/notes")]` | `src/repositories.rs` | Generated CRUD, a derived `find_by_pinned` finder, and generated REST read handlers mounted next to GraphQL |
| Repository from `AppState`, outside an extractor | `src/notes.rs` | `PgNoteRepository::with_pool_untracked(pool)` in resolvers — the constructor for code with a pool but no request |
| Run-once startup seed | `src/lib.rs` | One connection, one transaction, `pg_advisory_xact_lock` keyed like the framework's `Lock`: a scaled deployment seeds exactly once, and a `pool_size = 1` deployment never deadlocks on itself |
| Embedded migrations | `src/lib.rs`, `migrations/` | `embed_migrations!()`, applied on boot in development and by tests to their testcontainer |
| `AutumnError` → GraphQL error | `src/notes.rs` | 4xx messages on the field, 5xx redacted and logged; HTTP status in `extensions.status`, so a client can tell a 422 from a 503 |
| `with_lock` pessimistic toggle | `src/notes.rs` | `togglePinned` flips the flag under `SELECT … FOR UPDATE` so overlapping toggles serialise; a missing row is the helper's own 404 |
| `Plugin` with `nest` + `declare_plugin_routes` | `src/graphql_plugin.rs` | Mounts `POST /graphql`, `GET /graphql?query=…` (queries only — a mutation over `GET` is a `405`), `GET /graphql/sdl`; routes show in `autumn routes` with plugin attribution and satisfy `autumn routes audit` |
| `GraphqlPlugin::guard` | `src/graphql_plugin.rs` | Wraps the nested router in any tower layer (`RequireApiToken` in the test); declared routes become `Gated` |
| `ID` scalar for a `BIGSERIAL` key | `src/notes.rs` | GraphQL `Int` is 32-bit by contract, so the `i64` key crosses the wire as `ID` (a string) and is parsed back with a `400` on garbage |
| `PluginContract` + conformance harness | `src/graphql_plugin.rs`, `tests/graphql_api.rs` | Declares the `autumn-web` series; `run_conformance` proves attribution, prefix, collisions and contract in one test |
| Maud page shell + `asset_url` | `src/lib.rs` | Autumn renders the document; `asset_url` gives fingerprinted URLs in a release build with an asset manifest |
| Committed Vite bundle, fixed file names | `frontend/vite.config.ts` | `app.js` / `app.css` with no content hash, so the shell references them by name |
| CSRF under `prod` | `src/lib.rs`, `frontend/src/api.ts` | `CsrfToken` → `<meta name="csrf-token">` in the shell; the client sends `X-CSRF-Token`; tested with CSRF forced on |
| Single-binary deploy | `src/lib.rs`, `Cargo.toml` | `autumn build --embed` bakes the bundle into the binary via `embed_static!()` + `.embedded_static(..)`; `asset_url` switches to the embedded fingerprint manifest |
| Typed GraphQL client, no Apollo | `frontend/src/api.ts` | One `fetch` per operation, typed against `frontend/src/types.ts` |
| Schema drift gate | `tests/graphql_api.rs`, `schema.graphql` | The committed SDL must equal what the server serves — the TypeScript types are written against it |
| Two test tiers | `tests/graphql_api.rs` | No-Docker tests for the shell, SDL and plugin conformance; Docker tests over a shared Postgres testcontainer with the real migration applied |
| Chromium smoke | `tests/system/smoke.rs` | Real binary against a testcontainer Postgres: shell → bundle → GraphQL query → form mutation into the table, zero console errors |

## Prerequisites

- Rust 1.88.0+
- PostgreSQL (or Docker: `docker compose up -d` in this directory starts one on
  `localhost:5432` matching `autumn.toml`)

Node 20.19+ or 22.12+ (the range Vite 8 and `@vitejs/plugin-react` 5 declare in
`engines`) and npm are needed only to **change** the frontend.

## Quick start

From the **workspace root** (`autumn/`):

```bash
docker compose -f examples/react-graphql/docker-compose.yml up -d   # or point [database].url at your own
cargo run -p react-graphql
```

On boot in the development profile the app applies `migrations/` and seeds two
notes into the empty table. Open <http://127.0.0.1:3000>: the notes load over
GraphQL; add, pin, and delete notes from the page.

### Prove it works

```bash
# The React shell, rendered by Autumn:
curl -s http://127.0.0.1:3000/ | grep -o '<div id="root">'

# A query (POST, JSON body):
curl -s http://127.0.0.1:3000/graphql \
  -H 'content-type: application/json' \
  -d '{"query":"{ notes { id title pinned } }"}'
# => {"data":{"notes":[{"id":"2","title":"Welcome to Autumn Notes","pinned":true},{"id":"1","title":"Try the GraphQL endpoint","pinned":false}]}}
#    (`id` is the GraphQL `ID` scalar — a string on the wire; the REST handler below shows it as a JSON number)

# The same rows through the generated REST handler (a `Page` envelope):
curl -s http://127.0.0.1:3000/api/notes | head -c 120

# The GraphQL-over-HTTP GET form:
curl -s 'http://127.0.0.1:3000/graphql?query=%7B%20notes(pinnedOnly:true)%20%7B%20title%20%7D%20%7D'
# => {"data":{"notes":[{"title":"Welcome to Autumn Notes"}]}}

# A mutation — `#[normalize(trim)]` strips the padding before the INSERT:
curl -s http://127.0.0.1:3000/graphql \
  -H 'content-type: application/json' \
  -d '{"query":"mutation { createNote(input: {title: \"  From curl  \"}) { id title } }"}'
# => {"data":{"createNote":{"id":"3","title":"From curl"}}}

# The model's rule, surfaced as a GraphQL error with the HTTP status it would have carried:
curl -s http://127.0.0.1:3000/graphql \
  -H 'content-type: application/json' \
  -d '{"query":"mutation { createNote(input: {title: \"   \"}) { id } }"}'
# => {"data":null,"errors":[{"message":"title: title must be 1–120 characters", ..., "extensions":{"status":422}}]}

# The hook: the seeded welcome note is pinned, so it cannot be deleted:
curl -s http://127.0.0.1:3000/graphql \
  -H 'content-type: application/json' \
  -d '{"query":"mutation { deleteNote(id: \"2\") }"}'
# => {"data":null,"errors":[{"message":"note 2 is pinned; unpin it before deleting", ...}]}

# The same rules through the generated REST handlers — one repository, one set of hooks:
curl -s -X POST http://127.0.0.1:3000/api/notes -H 'content-type: application/json' \
  -d '{"title":"  From REST  ","body":"","pinned":false}'
# => {"id":4,"title":"From REST",...}          (trimmed by #[normalize])
curl -s -o /dev/null -w '%{http_code}\n' -X DELETE http://127.0.0.1:3000/api/notes/2
# => 422                                      (refused by the before_delete hook)

# The schema, for generating client types:
curl -s http://127.0.0.1:3000/graphql/sdl
```

> If `localhost` gives you something unexpected, use `127.0.0.1` explicitly:
> Autumn binds IPv4 loopback, and another process (Docker Desktop, for one)
> may hold the IPv6 side of port 3000.

## How a write travels

```
GraphQL mutation ─┐
REST POST         ┴─► PgNoteRepository::save(&NewNote)
                           │
                           ├─ #[normalize(trim)]   canonicalise (model)
                           ├─ before_create        validate + reject (hooks.rs)
                           ├─ INSERT … RETURNING   inside one transaction
                           └─ after_create         (unused here)
```

Two things are worth knowing about that pipeline, because they shape where
the rules live:

- **Normalisation runs before hooks.** A title of `"   "` is `""` by the time
  `before_create` sees it, so `#[validate(length(min = 1))]` rejects it. Put
  canonicalisation on the model, not in a hook.
- **`repo.save` does not run `#[validate]` by itself.** The generated REST
  `create` handler validates its payload before calling `save`; a resolver,
  a task, or a seed calling the repository directly gets no such check. That
  is why `NoteHooks::before_create` calls `validate()` — it makes the model's
  rules hold for every caller, and lets the hook fold the per-field messages
  into the error a GraphQL client will actually see.

Having hooks also moves `update` onto the hooked path, which loads the row,
merges the patch, normalises the merged model, and persists the normalised
draft; a plain repository with no hooks takes a blind `UPDATE` instead. See
`docs/guide/forms.md` and `docs/guide/hooks-and-transactions.md`.

## Single-binary deploy

The React bundle rides along in the binary:

```bash
autumn build --embed -p react-graphql
```

`--embed` fingerprints `static/` (writing `.autumn-manifest.json` and a
content-hashed copy of each file next to the originals — both are ignored by
git), then compiles with the `embed-assets` feature. `src/lib.rs` opts in with
`embed_static!()` and `.embedded_static(..)` behind that feature, so the
release binary serves `/static/app/app.c7bfed64.js` and friends from its own
bytes with `cache-control: immutable`, and `asset_url` in the shell resolves
to those hashed names.

A release binary runs under the `prod` profile, and two of this example's
choices need stating there rather than assumed:

- **Migrations.** Applying them at boot is a dev-profile default; in `prod`
  the framework leaves it to an explicit `autumn migrate` unless
  `[database] auto_migrate = true` — which this example's `autumn.toml`
  sets, because it is a single process. A multi-replica fleet should keep
  that off and run `autumn migrate` as a release step.
- **Trusted hosts.** `prod` refuses to start without a Host-header
  allow-list; `autumn.toml` lists the loopback names for the deploy below,
  and a real deployment lists its real hostname(s).
- **CSRF.** `prod` turns the framework's CSRF layer on. The shell renders
  the token as `<meta name="csrf-token">` (via the `CsrfToken` extractor —
  the cookie is `HttpOnly`, so this is how page script learns it) and the
  client echoes it in `X-CSRF-Token` on every mutation: the same
  double-submit contract the framework's htmx helper uses. In dev the tag is
  absent and CSRF is off. `react_app_works_under_the_prod_profile` runs the
  whole browser journey under `prod` to prove it.
- **The public write API.** The framework refuses to start in `prod` when a
  `#[repository(api = …)]` exposes `POST`/`PUT`/`DELETE` without a
  `policy = …`. This example's API is deliberately unauthenticated, so
  `autumn.toml` opts out with `[security] allow_unauthorized_repository_api
  = true`; a real app adds a policy (and guards the GraphQL mount) instead.

So the one-file deploy is the binary **plus its `autumn.toml`** (or the
equivalent `AUTUMN_*` variables), plus the one thing every Autumn app needs
in `prod` and must never have in a file: a signing secret. Copy them
anywhere, point `AUTUMN_DATABASE__URL` at a Postgres, and it migrates, seeds,
and serves:

```bash
mkdir /tmp/deploy && cp target/release/react-graphql examples/react-graphql/autumn.toml /tmp/deploy/
cd /tmp/deploy && \
  AUTUMN_SECURITY__SIGNING_SECRET=$(openssl rand -hex 32) \
  AUTUMN_DATABASE__URL=postgres://autumn:autumn@localhost:5432/notes \
  ./react-graphql
```

Leave the secret out and `prod` refuses to start, naming the variable; leave
`allow_unauthorized_repository_api` out and it refuses to start, naming the
unguarded `POST`/`PUT`/`DELETE` routes. Both refusals are the framework
doing its job; the config above is this example choosing to be public.

A dev build never enables the feature, so `npm run build` output is picked up
from disk without recompiling Rust.

## Frontend development

The committed bundle is what `cargo run` serves. To work on the React app:

```bash
cd examples/react-graphql/frontend
npm install
npm run dev        # Vite dev server on :5173, proxies /graphql to :3000
```

Keep `cargo run -p react-graphql` running in another terminal; the Vite dev
server proxies every `/graphql` call to it (see `vite.config.ts`), so the
browser still talks same-origin and no CORS configuration is needed. Edits to
components hot-reload without touching the Rust process.

### Rebuilding the committed bundle

```bash
cd examples/react-graphql/frontend
npm run build      # typecheck, then write ../static/app/app.js + app.css
```

Commit the result. The bundle is a build product checked in on purpose, the
same way `examples/flock` commits its wasm artifacts: it keeps the example
runnable, testable, and smoke-able with nothing but `cargo`.

### Keeping the types honest

`frontend/src/types.ts` is a hand-written mirror of `schema.graphql`, and
`schema.graphql` is drift-tested against what the server serves. When the
schema changes:

```bash
cargo run -p react-graphql &
curl -s http://127.0.0.1:3000/graphql/sdl > examples/react-graphql/schema.graphql
```

then update `types.ts` to match. A larger schema would run GraphQL Code
Generator against that same endpoint instead.

## How the plugin works

```rust
autumn_web::app()
    .migrations(MIGRATIONS)
    .routes(routes())                                  // shell + generated REST reads
    .plugin(GraphqlPlugin::new(notes::build_schema()).path("/graphql"))
    .on_startup(|state| async move { seed_if_empty(&state).await })
    .run()
    .await;
```

`GraphqlPlugin::build` does three things with the `AppBuilder` it is handed:

| Call | Why |
|------|-----|
| `nest("/graphql", router)` | Mounts a raw axum router: `POST /` and `GET /` execute, `GET /sdl` prints the schema. The schema rides on that router as an `axum::Extension` layer — router-local state — so a second `GraphqlPlugin` at another path keeps its own even when both share root types |
| `declare_plugin_routes(routes)` | A nested router is opaque to `autumn routes`; declaring makes the routes visible, attributed to the plugin, and audit-clean |
| `contract()` → `PluginContract` | Names the plugin, its version, and the `autumn-web` series it supports |

Each execution runs `schema.execute(request.data(state))`, so every resolver
can call `ctx.data::<AppState>()` and from there `state.pool()`,
`state.extension::<T>()`, or anything else a handler could. No second
registry. `with_pool_untracked` is the repository constructor for exactly
that situation — code with a pool but no request; in a route handler you
would take `repo: PgNoteRepository` as an argument instead.

The plugin ships inside this example for readability. It has no dependency
on the surrounding crate, so lifting it into a published
`autumn-plugin-graphql` crate is a copy of one file plus a `Cargo.toml`.

### Why no GraphiQL

`async-graphql` can serve a GraphiQL page, but that page loads its scripts
from a CDN, which Autumn's default `script-src 'self'` policy blocks. Rather
than loosen the CSP for a dev tool, the plugin serves the SDL at
`/graphql/sdl`; point any GraphQL IDE at `/graphql` and paste the schema.

### Guarding the endpoint

Unguarded, every route the plugin declares is classified `Public`. To require
a bearer token, hand the framework's layer to the plugin:

```rust
use autumn_web::auth::{DbApiTokenStore, RequireApiToken};

GraphqlPlugin::new(schema)
    .guard(RequireApiToken::new(Arc::new(DbApiTokenStore::new(pool))), "RequireApiToken")
```

`guard` exists because `AppBuilder::scoped(prefix, layer, routes![…])` wraps
only the routes handed to it — a raw router a plugin nests is never among
them, so a `.scoped(...)` around the app's own routes would leave
`POST /graphql` open. The plugin applies the layer as the outermost on its
router, so it runs before every handler including `GET /sdl`, and the
declared routes flip to `Gated` with the label as their middleware in
`autumn routes`. The `guard_layer_protects_every_plugin_route` test shows it
with `InMemoryApiTokenStore`. (A global `AppBuilder::layer` guards
everything; `guard` is for guarding just this mount.)

## Tests

```bash
# Tier 1 — no Docker: shell, SDL drift, plugin conformance, error mapping.
cargo test -p react-graphql

# Tier 2 — Docker: rows, hooks, normalisation, validation, REST/GraphQL parity.
# (CI runs this tier in the Docker job of ci.yml.)
# Serial, because they share one migrated table.
cargo test -p react-graphql --test graphql_api -- --include-ignored --test-threads=1

# Headless-Chromium smoke (requires Chromium + Docker; runs in the fleet e2e gate):
cargo test -p react-graphql --features system-tests --test smoke -- --include-ignored

# Frontend typecheck:
cd frontend && npm run typecheck
```

## Available routes

| Method | Path | Source | Response |
|--------|------|--------|----------|
| GET | `/` | app | The page shell React mounts into |
| GET | `/api/notes` | `#[repository(api)]` | Paged JSON list of notes (`content`, `page`, `total_elements`, …) |
| GET | `/api/notes/{id}` | `#[repository(api)]` | One note as JSON, or 404 |
| POST | `/api/notes` | `#[repository(api)]` | Create from `{title, body, pinned}`; validated, trimmed, hooked like `createNote` |
| PUT | `/api/notes/{id}` | `#[repository(api)]` | Partial update (`Patch` fields) |
| DELETE | `/api/notes/{id}` | `#[repository(api)]` | Delete; a pinned note is a 422 from the same `before_delete` hook |
| GET | `/static/app/app.js`, `/static/app/app.css` | framework | The committed Vite bundle |
| POST | `/graphql` | plugin | Execute `{ query, variables?, operationName? }` |
| GET | `/graphql?query=…` | plugin | Execute a read in query-string form; mutations are refused with `405` |
| GET | `/graphql/sdl` | plugin | The schema as `text/plain` SDL |
| GET | `/health` | framework | Liveness |
