# Getting Started with Autumn

This guide takes you from an empty directory to a running Autumn application
with routes, a Postgres-backed model, HTML templates, styling, and
interactivity. Budget about 30 minutes.

Autumn is a convention-over-configuration web framework for Rust, built on
[Axum](https://github.com/tokio-rs/axum). It assembles Diesel (database), Maud
(HTML), Tailwind CSS (styling), htmx (interactivity), health and actuator
endpoints, profile-aware configuration, and a project CLI behind a Spring
Boot-style developer experience.

> **Version note:** This guide tracks the published `autumn-web` and
> `autumn-cli` **0.7.x** release line. If you are working from a source
> checkout of the Autumn repository, the workspace may be ahead of the
> published crates — see [local development](#local-development) below.

**Where this guide sits.** It is the fast tour. If you want the long-form
build with checkpoints, work through the
[tutorial](tutorial/index.md) instead (~2 hours). If you already know your way
around Rust web frameworks and just want CRUD on screen, jump straight to
[generate a resource](#the-fast-path-generate-a-crud-resource).

---

## Prerequisites

- **Rust 1.88.0+** (edition 2024) — install via [rustup](https://rustup.rs/)
- **The PostgreSQL client library (`libpq`)** — required to *build*, not just
  to run. The `db` feature is on by default, so a fresh project links `libpq`
  even before you configure a database. Install `libpq-dev` (Debian/Ubuntu),
  `libpq` (Homebrew), or `postgresql-devel` (Fedora/RHEL).
- **A reachable PostgreSQL server** — needed from
  [Add a database](#add-a-database) onward. The first run works without one;
  there is a Docker one-liner below when you get there.
- **The Diesel CLI** — `autumn migrate` shells out to it. Install it once:
  ```bash
  cargo install diesel_cli --no-default-features --features postgres
  ```

Verify your toolchain:

```bash
rustc --version   # 1.88.0 or later
cargo --version
```

Two scaffold flavors change that list, and they change it in different
directions — pick by what you want to *drop*, not by what you want to build:

> **No database at all?** `autumn new my-app --daemon` scaffolds a model-free
> app with `autumn-web`'s `db` feature switched off, so it needs neither
> Postgres nor `libpq` nor the Diesel CLI. It keeps the view stack, so
> everything below except the database sections applies. (`--bundled-pg` is
> the opposite trade: a daemon that *keeps* the database and manages a local
> Postgres for you.)
>
> **Building a JSON API?** `autumn new my-app --api` drops the view stack —
> Maud, Tailwind, htmx, the whole `static/` tree — but **keeps** the database
> and migrations. The `libpq` prerequisite above still applies to it; the HTML,
> Tailwind, and htmx sections of this guide do not. The two flags are not
> combinable.

---

## Install the CLI

The fastest route is a prebuilt binary — no Rust toolchain needed for the CLI
itself:

```sh
# macOS & Linux
curl -fsSL https://raw.githubusercontent.com/autumn-foundation/autumn/trunk-dev/scripts/install.sh | sh
```

```powershell
# Windows
irm https://raw.githubusercontent.com/autumn-foundation/autumn/trunk-dev/scripts/install.ps1 | iex
```

Both installers verify a SHA-256 checksum before installing. See the
[README](../../README.md) for pinning a version or choosing an install
directory.

To build it from crates.io instead:

```bash
cargo install autumn-cli --version 0.7.0
```

### Local development

For local development only, from an Autumn source checkout, install the CLI you
just built:

```bash
cargo install --path autumn-cli
```

Between releases the workspace can be ahead of the published crates, so a
source-built CLI may scaffold projects pinning an `autumn-web` version that is
not on crates.io yet. `autumn doctor`'s `version_compat` check reports the two
versions side by side; if they disagree, either point the generated
`Cargo.toml` at your checkout with a `[patch.crates-io]` override or install
the published CLI instead.

Either way you get the `autumn` binary. These are the commands you will touch in
your first hour:

| Command                    | What it does                                              |
|----------------------------|-----------------------------------------------------------|
| `autumn doctor`            | Diagnose the environment before your first run            |
| `autumn new`               | Scaffold a new project                                    |
| `autumn setup`             | Download the Tailwind CSS binary (checksum-verified)      |
| `autumn dev`               | Run the dev server with file watching and live reload     |
| `autumn generate`          | Scaffold a model, migration, CRUD routes, and views       |
| `autumn db`                | Create, drop, or reset the database itself                |
| `autumn migrate`           | Apply migrations or inspect migration status              |
| `autumn routes`            | List every mounted route, its handler, and its middleware |
| `autumn test`              | Provision the test database, migrate it, run `cargo test` |
| `autumn console`           | Open a pre-wired data playground against your database    |

That is a small slice. `autumn --help` lists the full set — deployment
(`release`, `deploy`), operations (`monitor`, `maintenance`, `canary`,
`flags`), diagnostics (`replay`, `export`), and more.

---

## Create a project

```bash
autumn new my-app
cd my-app
```

That writes:

```
my-app/
  Cargo.toml
  README.md                 # project quickstart, generated for this app
  autumn.toml               # framework configuration
  Dockerfile                # production container image
  .dockerignore
  .env.example              # copy to .env for local secrets/overrides
  .gitignore
  build.rs                  # Tailwind CSS build pipeline + build provenance
  rust-toolchain.toml
  rustfmt.toml
  clippy.toml
  deny.toml                 # dependency advisory policy — CI audits against it
  tailwind.config.js        # Tailwind content globs
  .autumn/scaffold.toml     # which release scaffolded this — commit it
  src/
    main.rs                 # your application entry point
  static/
    css/input.css           # Tailwind entry point
    js/htmx.min.js          # vendored, integrity-pinned
    js/htmx-ext-sse.min.js
    .autumn-assets.json     # vendored-asset manifest with SRI hashes
  migrations/               # Diesel migrations (empty for now)
  tests/
    integration_test.rs     # working smoke tests, no Docker required
  config/
    master.key              # encrypts credentials — keep secret, never commit
    credentials/development.toml.enc
  .github/workflows/ci.yml  # fmt, clippy, dependency audit, test, a11y checks
```

`.autumn/scaffold.toml` is bookkeeping, but commit it: it records which
release's scaffold produced the framework-owned files above, so a later
[`autumn upgrade`](upgrading.md#scaffold-files) can tell a template that moved
from a file you edited, and never overwrite your work.

A sibling file, `.autumn/generated.toml`, appears after your first `autumn
generate`. Commit it too: it does the same job for generated code, so
[`autumn destroy`](generators.md#undoing-a-generator-autumn-destroy) can tell
its own output from your edits.

The files that matter right now:

| Path                       | Purpose                                          |
|----------------------------|--------------------------------------------------|
| `src/main.rs`              | Routes and application bootstrap                 |
| `autumn.toml`              | Server, database, logging, probes, telemetry     |
| `.env.example`             | Template for local env overrides (copy to `.env`)|
| `build.rs`                 | Compiles Tailwind CSS on `cargo build`           |
| `static/`                  | Auto-served at `/static/`                        |
| `migrations/`              | Diesel SQL migrations                            |
| `tests/`                   | Integration tests against the real middleware stack |
| `config/master.key`        | Decrypts `config/credentials/*.enc` — gitignored |

---

## Preflight with `autumn doctor`

Run this from the project root before anything else. It checks the environment
and the project config together, and tells you what to fix instead of leaving
you to decode a runtime error:

```bash
autumn doctor
```

On a fresh project, before `autumn setup`, you will see something like:

```
🍂 autumn doctor

✅ rust_toolchain — rustc 1.88.0 ≥ MSRV 1.88.0
✅ version_compat — autumn-cli 0.7.0 matches autumn-web 0.7.0
✅ autumn_toml — autumn.toml and profile configurations are valid
✅ database_topology — database not configured
✅ port_bindable — port 3000 is available
❌ tailwind_binary — target/autumn/tailwindcss not found
   hint: Run `autumn setup` to download the Tailwind CSS binary
⚠️  signing_secret — using an ephemeral per-process signing secret (dev/test
    only; sessions and signed URLs will not survive restarts or be shared
    across replicas)
   hint: Set AUTUMN_SECURITY__SIGNING_SECRET before deploying to production
⚠️  dotenv — `.env.example` is present but no `.env` exists
   hint: Copy `.env.example` to `.env` and fill in local values
... (about twenty more checks: TLS, trusted hosts, rate limiting, proxy
     config, backups, mail, jobs, alerting, model privacy)

23 passed, 3 warnings, 1 failed — problems found
```

Every failing or warning check prints a one-line remediation hint beneath it.
Warnings on a fresh project are expected — they are the production checklist,
not first-run blockers. The exact tally varies with what the machine has
installed (Chromium for system tests, `pg_dump`/`pg_restore`) and with what you
have configured, so treat the counts above as illustrative.

**Exit codes:** `0` when every check passes (warnings allowed), `1` when any
check fails. Two flags matter in automation:

```bash
autumn doctor --strict   # treat warnings as failures — good CI pre-flight gate
autumn doctor --json     # machine-readable output
```

Clear the Tailwind failure now:

```bash
autumn setup
```

This downloads the platform-specific Tailwind CSS v4 standalone binary to
`target/autumn/tailwindcss` and verifies its SHA-256 checksum. Re-run with
`--force` to re-download.

---

## Run it

```bash
autumn dev
```

`autumn dev` watches `src/`, `static/`, `templates/`, `migrations/`, and the
project's top-level config files, rebuilding and reloading the browser on
change. To run without watch mode:

```bash
cargo run
```

You will see log output like:

```
  INFO autumn: Database not configured
  INFO autumn: Listening bound=127.0.0.1:3000
```

Visit <http://localhost:3000> — you should see "Welcome to my-app!". These
routes are live immediately:

| Path                  | What it is                                              |
|-----------------------|---------------------------------------------------------|
| `/`                   | The scaffolded welcome page                             |
| `/hello`              | A plain-text handler                                    |
| `/hello/world`        | A path-parameter handler                                |
| `/health`             | Health check                                            |
| `/actuator/health`    | Actuator health, with component detail                  |
| `/actuator/info`      | Build provenance — git SHA, branch, build timestamp     |
| `/actuator/metrics`   | Runtime metrics                                         |
| `/live` `/ready` `/startup` | Kubernetes-style probes                           |

`/health` responds with:

```json
{ "status": "ok", "version": "0.7.0" }
```

Press **Ctrl+C** to stop the server. Shutdown is graceful, draining in-flight
requests up to `server.shutdown_timeout_secs`.

---

## The generated app

Open `src/main.rs`. The scaffold is small but deliberately not a bare
hello-world — it shows the conventions you will follow as the app grows. Here
is its skeleton, with the layout helper, the cookie-consent flow, and the
asset-embedding hooks elided:

```rust
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::prelude::*;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

#[get("/")]
#[public]
async fn index(flash: Flash, path: CurrentPath) -> maud::Markup {
    layout("Welcome", path.as_str(), flash_messages(&flash.consume().await), maud::html! {
        h1 { "Welcome to my-app!" }
        p { "Edit " code { "src/main.rs" } " to get started." }
    })
}

#[get("/hello")]
#[public]
async fn hello() -> &'static str {
    "Hello, Autumn!"
}

#[get("/hello/{name}")]
#[public]
async fn hello_name(name: autumn_web::extract::Path<String>) -> String {
    format!("Hello, {}!", *name)
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![index, hello, hello_name])
        .migrations(MIGRATIONS)
        .run()
        .await;
}
```

The pieces:

- **`#[get("/path")]`** annotates a handler for GET requests. Also available:
  `#[post]`, `#[put]`, `#[patch]`, `#[delete]`, plus `#[static_get]` for routes
  pre-rendered to `dist/` by `autumn build`.
- **`#[public]`** marks a route as deliberately unauthenticated. Autumn can
  prove authentication coverage at build time — `autumn routes audit` fails on
  any route that is neither framework-owned, guarded by `#[secured]` /
  `#[authorize]`, nor explicitly `#[public]`. See
  [route auth coverage](route-auth-coverage.md).
- **`routes![...]`** collects annotated handlers into a `Vec<Route>`.
- **`autumn_web::app().routes(...).run().await`** is the app builder: load
  config, create the database pool, mount routes, start the server.
- **`.migrations(MIGRATIONS)`** embeds the app's Diesel migrations in the
  binary. On the `dev` profile they are applied at startup; every other profile
  is opt-in via `database.auto_migrate` (see
  [migrations](migrations.md)).
- **`#[autumn_web::main]`** sets up the Tokio runtime — a thin wrapper around
  `#[tokio::main]` that also records the build profile. It takes optional
  arguments for tuning that runtime (see
  [tuning the Tokio runtime](#tuning-the-tokio-runtime) below).

Handlers are ordinary async functions. They can return anything Axum can turn
into a response: `&str`, `String`, `Json<T>`, `Markup` (Maud HTML), or your own
`impl IntoResponse`.

What the excerpt above leaves out is worth opening the file for. The scaffold's
`layout()` wires in a nav bar with `aria-current` tracking, a skip-to-content
link, flash messages, and the framework's widget CSS. Alongside it is a working
cookie-consent flow — a banner injected by a `.layer(...)` on the builder,
CSRF-protected accept/reject routes, and a preferences page linked from the
footer so withdrawing consent is as easy as giving it — see the
[cookie-consent guide](./cookie-consent.md) for how to gate your own
non-essential cookies on it, since the banner alone is not the compliance.
There is also a `#[cfg(feature = "embed-assets")]` block that bakes `static/`
into the binary for `autumn build --embed`. All of it is ordinary user code:
delete what you do not need.

To see everything that is actually mounted, including framework routes and
per-route middleware:

```bash
autumn routes
```

---

## Routing essentials

Path parameters use curly braces in the pattern and the `Path<T>` extractor in
the signature:

```rust
use autumn_web::extract::Path;
use autumn_web::{get, public};

#[get("/users/{id}")]
#[public]
async fn get_user(id: Path<i64>) -> String {
    format!("User #{}", *id)
}
```

> **Why `#[public]` on every example from here on.** The scaffold's generated
> `.github/workflows/ci.yml` runs `autumn routes audit`, which fails the build
> on any mounted route carrying no security classification. A handler with no
> `#[public]`, `#[secured]`, or `#[authorize]` is *unclassified*, not
> *public* — it works locally and goes red on your first CI push. Saying
> `#[public]` out loud is the whole point of the gate: an unauthenticated route
> should be a decision, not an oversight.

Multiple parameters destructure a tuple:

```rust
#[get("/orgs/{org}/repos/{repo}")]
#[public]
async fn get_repo(Path((org, repo)): Path<(String, String)>) -> String {
    format!("{org}/{repo}")
}
```

Query strings use `Query<T>` over any `Deserialize` type:

```rust
use autumn_web::extract::Query;
use serde::Deserialize;

#[derive(Deserialize)]
struct Pagination {
    page: Option<u32>,
    per_page: Option<u32>,
}

#[get("/items")]
#[public]
async fn list_items(Query(params): Query<Pagination>) -> String {
    let page = params.page.unwrap_or(1);
    let per_page = params.per_page.unwrap_or(20);
    format!("Page {page}, showing {per_page} items")
}
```

Compose route groups by calling `.routes()` more than once — one call per
module keeps `main.rs` readable as the app grows:

```rust
autumn_web::app()
    .routes(routes![index, hello, hello_name])
    .routes(routes![list_items, get_user, get_repo])
    .run()
    .await;
```

To mount a group under a path prefix *and* wrap it in a Tower layer — the usual
shape for a guarded API surface — use `.scoped(prefix, layer, routes)`. Unlike
a raw merged router, scoped routes stay in the route registry, so
`autumn routes`, the auth audit, and OpenAPI still see them:

```rust
use autumn_web::middleware::RequestIdLayer; // any Tower Layer

autumn_web::app()
    .routes(routes![index])
    .scoped("/api/v1", RequestIdLayer::default(), routes![list_items])
    .run()
    .await;
```

---

## The fast path: generate a CRUD resource

Before writing CRUD by hand, know that you usually do not have to.
`autumn generate scaffold` emits the model, the migration, the `schema.rs`
entry, a `#[repository]`, HTML routes and views, a policy stub, a JSON API, and
a smoke test — then wires the routes into `src/main.rs` for you:

```bash
autumn new my-app
cd my-app
autumn generate scaffold Post title:String body:Text published:bool
# configure [database] in autumn.toml (next section), then:
autumn db create
autumn migrate
autumn dev
```

Visit <http://localhost:3000/posts> for the generated index page, or
<http://localhost:3000/api/posts> for the JSON read endpoint.

### The writes are locked, on purpose

Those five commands do **not** hand you an open CRUD app, and it is worth
understanding why before you go looking for the bug:

- The read paths — `GET /posts`, `GET /posts/{id}`, the CSV export, and the
  JSON `GET /api/posts` — work immediately.
- The write paths — `new_form`, `create`, `edit_form`, `update`, `destroy`,
  `bulk_delete` — are locked twice over. Each is emitted with `#[secured]`, so
  a request with no authenticated session is rejected at the route; and each
  handler body *also* calls `authorize::<Post>(&state, &session, "update", &row)`
  (or `authorize_create::<Post>`) against the `PostPolicy` registered in
  `src/main.rs`. A fresh `autumn new` app has no login flow, so you get a
  `401` at the route and, if you strip the attribute, a `403` from the policy.
- The JSON *write* handlers are generated but deliberately **not** registered
  in `routes![]`. Mount them once you have written a real repository policy.

That double gate is deliberate. `#[secured]` answers "is anyone signed in?" and
the policy answers "may *this* user touch *this* row?" — deleting one attribute
cannot silently open a mutation path, and `autumn routes audit` enforces the
same posture in CI. It is the scaffold preferring to hand you a locked door
over an unauthenticated `DELETE` endpoint you forgot about.

To get writes working, generate the missing half — a full signup, login,
logout, account, and password-reset flow, named after the model it creates:

```bash
autumn generate auth User
autumn migrate
autumn dev
```

That writes the `users` migration, the session and remember-token models, the
handlers, and registers all of them in `src/main.rs`.

Signing up is two steps, not one — the generated flow does not log you in:

1. Sign up at <http://localhost:3000/signup>. The account is created
   **unconfirmed**, and login rejects unconfirmed accounts.
2. Open the confirmation link. On the `dev` profile Autumn defaults the mail
   transport to `log` when `[mail]` is unset, so the confirmation email is
   printed straight into your `autumn dev` output. Copy the
   `/auth/confirm/{token}` URL out of the terminal and visit it.

Then log in, and the create/edit/delete views are reachable.

> **On any non-`dev` profile** that smart default does not apply: with `[mail]`
> unset the transport is `Disabled`, and signup fails fast with a 500 rather
> than creating an account nobody can confirm. Set `[mail] transport` (use
> `"log"` locally, `"smtp"` in production) before running the flow there. Same
> for forgot-password and change-email, which are also mail-gated. See the
> [mail guide](mail.md).

Read the generated `src/policies/post.rs` before you ship: with no owner column
detected it authorizes *any* authenticated user and says so in `SECURITY TODO`
comments. Replace those with a real per-record ownership rule. See
[authentication](authentication.md) and [authorization](authorization.md).

Generating the auth flow really is the short path here. Opening the writes to
anonymous visitors instead means editing both layers — dropping every
`#[secured]` attribute *and* rewriting `PostPolicy` to return `true` — which is
more work than `autumn generate auth User`, and leaves you with a scaffold that
no longer resembles the one the rest of the docs describe.

### More of the DSL

The field DSL carries validation and relationships too — `title:String{min=3,max=120}`
emits both a server-side `#[validate(length(...))]` rule and the matching HTML5
constraint; `body:richtext` swaps the textarea for a sanitizing Markdown editor;
`post:references` with `--belongs-to Post` scaffolds the parent's show page with
its children listed and an inline create form.

Generated code uses only macros and conventions Autumn already ships, so once a
generator has run the files are ordinary user code you should edit freely.
`autumn destroy` cleanly reverses a matching `generate` invocation. See the
[code generators guide](generators.md) for the full field DSL, every
subcommand, and the flags.

The rest of this guide builds that same shape by hand — a `Todo` resource,
step by step — because knowing what the generator emits is what makes the
generated code safe to edit.

---

## Add a database

Autumn uses [Diesel](https://diesel.rs/) with
[diesel-async](https://github.com/weiznich/diesel_async) and
[deadpool](https://docs.rs/deadpool) for async Postgres connections.

### 1. Configure the connection

`autumn.toml` ships with the `[database]` block commented out. Uncomment it and
point it at a reachable Postgres:

```toml
[database]
url = "postgres://postgres:postgres@localhost:5432/my_app"
pool_size = 10
connect_timeout_secs = 5
```

`url` is the single-primary compatibility field. For production-shaped config,
name the write role explicitly:

```toml
[database]
primary_url = "postgres://localhost/my_app"
# replica_url = "postgres://localhost:5433/my_app"
primary_pool_size = 10
replica_pool_size = 5
replica_fallback = "fail_readiness"
auto_migrate = false
```

`Db`, transactions, advisory locks, and `autumn migrate` always use the primary
role. The optional replica role serves read paths that tolerate replay lag,
governed by `replica_fallback`.

Prefer environment variables? Copy `.env.example` to `.env` (already
gitignored) and set the URL there instead:

```bash
AUTUMN_DATABASE__PRIMARY_URL="postgres://localhost/my_app"
```

`autumn dev` auto-loads `.env` on the dev and test profiles, and real shell
environment variables always win over `.env` values. Note the double
underscore `__` separating section from field.

### 2. Get a Postgres

Already have one? Skip ahead. Otherwise start a throwaway instance matching the
URL above:

```bash
docker run -d --rm --name my-app-pg -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_DB=my_app -p 5432:5432 postgres:16

# The container reports "started" before it accepts connections — wait for it.
until docker exec my-app-pg pg_isready -U postgres >/dev/null 2>&1; do sleep 1; done
```

### 3. Create the database

```bash
autumn db create
```

This reads the connection you configured and creates the database on its
server. It is idempotent — run it again and it reports that the database
already exists.

### 4. Write a migration

```bash
autumn generate migration CreateTodos
```

That emits a timestamped directory under `migrations/` with `up.sql` and
`down.sql`. Edit `up.sql`:

```sql
CREATE TABLE todos (
    id BIGSERIAL PRIMARY KEY,
    title TEXT NOT NULL,
    completed BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);
```

And `down.sql`:

```sql
DROP TABLE todos;
```

`BIGSERIAL` / `i64` is the Autumn primary-key convention — the generators, the
`#[repository]` macro, and the admin plugin all assume it.

Apply it:

```bash
autumn migrate
```

`autumn migrate` applies every pending migration to the primary database and
regenerates `src/schema.rs` with Diesel's table macro.

> **Tip — reset the dev database.** While iterating on a schema, `autumn db
> reset` drops, recreates, migrates, and (when `src/bin/seed.rs` exists) seeds
> in one step. It refuses to run against a production profile without
> `--force`.

---

## Define a model

Create `src/models.rs`:

```rust
use diesel::prelude::*;
use serde::{Deserialize, Serialize};

use crate::schema::todos;

#[derive(Queryable, Selectable, Serialize)]
#[diesel(table_name = todos)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Todo {
    pub id: i64,
    pub title: String,
    pub completed: bool,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Insertable, Deserialize)]
#[diesel(table_name = todos)]
pub struct NewTodo {
    pub title: String,
}
```

Autumn's `#[model]` attribute macro derives the Diesel and Serde traits for
you:

```rust
use crate::schema::todos;

// Equivalent to the manual derives above (Queryable, Selectable, Insertable,
// Serialize, Deserialize) plus #[diesel(table_name = todos)].
#[autumn_web::model(table = "todos")]
pub struct Todo {
    pub id: i64,
    pub title: String,
    pub completed: bool,
    pub created_at: chrono::NaiveDateTime,
}
```

Omit `table = "..."` and the table name is inferred from the struct name:
`BlogPost` becomes `blog_posts`, `User` becomes `users`.

For a full data-access layer — typed finders, pagination, bulk CRUD, hooks,
soft delete, counter caches — annotate a trait with `#[repository]` instead of
hand-writing queries. See the [repositories guide](repositories.md).

Writing Diesel code by hand means naming Diesel directly. Add these alongside
the dependencies `autumn new` already wrote — the scaffold depends only on
`autumn-web`, `maud`, and `diesel_migrations`, because until now nothing in
your code referred to Diesel's traits:

```toml
[dependencies]
autumn-web = "0.7"
chrono = { version = "0.4", features = ["serde"] }
diesel = { version = "2", features = ["postgres", "chrono"] }
diesel-async = { version = "0.9", features = ["postgres"] }
serde = { version = "1", features = ["derive"] }
```

(`autumn generate scaffold` adds these for you — another reason to let the
generator lay down the first resource.)

Declare the modules in `main.rs`:

```rust
mod models;
mod schema;
```

---

## Query the database

Use the `Db` extractor to take an async connection from the pool. It
dereferences to the runtime connection type, so `&mut *db` goes straight into
Diesel queries:

```rust
use autumn_web::prelude::*;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::models::{NewTodo, Todo};
use crate::schema::todos;

#[get("/todos")]
#[public]
async fn list_todos(mut db: Db) -> AutumnResult<Json<Vec<Todo>>> {
    let all_todos = todos::table
        .order(todos::created_at.desc())
        .select(Todo::as_select())
        .load(&mut *db)
        .await?;

    Ok(Json(all_todos))
}

#[post("/api/todos")]
#[public]
async fn create_todo(mut db: Db, body: Json<NewTodo>) -> AutumnResult<Json<Todo>> {
    let created: Todo = diesel::insert_into(todos::table)
        .values(&body.0)
        .returning(Todo::as_returning())
        .get_result(&mut *db)
        .await?;

    Ok(Json(created))
}
```

Key points:

- **`Db` is an extractor, not a global.** Declare it in the handler signature
  and Autumn hands you a pooled connection, returned to the pool when the
  handler completes.
- **`AutumnResult<T>` is `Result<T, AutumnError>`.** The `?` operator converts
  any `std::error::Error` into an `AutumnError` with status 500 — Diesel
  errors, I/O errors, serde errors all work.
- **`mut db: Db`** — Diesel queries take `&mut` on the connection.

Register the handlers and try it:

```rust
autumn_web::app()
    .routes(routes![list_todos, create_todo])
    .migrations(MIGRATIONS)
    .run()
    .await;
```

```bash
curl -X POST http://localhost:3000/api/todos \
  -H "Content-Type: application/json" \
  -d '{"title": "Write Autumn guide"}'

curl http://localhost:3000/todos
```

Need a transaction across several statements? Use `Db::tx` / `Db::tx_with` —
see [transactions](transactions.md).

---

## Render HTML with Maud

Autumn re-exports [Maud](https://maud.lambda.xyz/), a compile-time HTML
templating library. Return `Markup` from a handler to send HTML:

```rust
use autumn_web::prelude::*;

#[get("/")]
#[public]
async fn index() -> Markup {
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { "My App" }
                link rel="stylesheet" href=(asset_url("css/autumn.css"));
            }
            body {
                h1 { "Welcome to my app" }
                p { "Built with Autumn." }
            }
        }
    }
}
```

Maud syntax in brief:

| Maud                              | HTML output                      |
|-----------------------------------|----------------------------------|
| `h1 { "Hello" }`                  | `<h1>Hello</h1>`                 |
| `div class="box" { "content" }`   | `<div class="box">content</div>` |
| `input type="text" name="q";`     | `<input type="text" name="q">`   |
| `(variable)`                      | Escaped interpolation            |
| `(PreEscaped(raw_html))`          | Unescaped interpolation          |
| `@if cond { ... } @else { ... }`  | Conditional rendering            |
| `@for item in &items { ... }`     | Loop rendering                   |

Extract reusable layouts into functions:

```rust
use autumn_web::HTMX_CSRF_JS_PATH;
use autumn_web::prelude::*;
use autumn_web::security::CsrfToken;

fn layout(title: &str, csrf_token: Option<&str>, content: Markup) -> Markup {
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                // htmx reads the token from here. The CSRF cookie is HttpOnly,
                // so JavaScript cannot get at it any other way.
                @if let Some(token) = csrf_token {
                    meta name="csrf-token" content=(token);
                }
                link rel="stylesheet" href=(asset_url("css/autumn.css"));
                (javascript_include_tag("htmx"))
                script src=(HTMX_CSRF_JS_PATH) {}
            }
            body class="bg-gray-100 min-h-screen" {
                div class="max-w-2xl mx-auto py-10 px-4" {
                    (content)
                }
            }
        }
    }
}

#[get("/about")]
#[public]
async fn about(csrf: Option<CsrfToken>) -> Markup {
    layout("About", csrf.as_ref().map(CsrfToken::token), html! {
        h1 { "About this app" }
        p { "Built with Autumn, Maud, and Tailwind." }
    })
}
```

Those three CSRF lines are load-bearing the moment you add htmx. `HTMX_CSRF_JS_PATH`
is a helper Autumn serves that copies the `csrf-token` meta tag into an
`X-CSRF-Token` header on every htmx request. Without it, the `hx-post` and
`hx-delete` calls in the next section sail through in `dev` and return
`403 Forbidden` under `prod`, where CSRF protection is on by default. The
`examples/reddit-clone` layout is the reference implementation.

`asset_url` and `javascript_include_tag` resolve to plain paths in dev and to
fingerprinted, integrity-pinned URLs after `autumn build` — so you get cache
busting without hardcoding hashes.

---

## Style with Tailwind CSS

Autumn integrates [Tailwind CSS](https://tailwindcss.com/) v4 through the
generated `build.rs`, which runs the Tailwind standalone CLI at compile time.

You already downloaded the binary with `autumn setup`. Write Tailwind utility
classes directly in your Maud templates — `build.rs` scans `src/**/*.rs` for
class names:

```rust
html! {
    div class="max-w-2xl mx-auto py-10 px-4" {
        h1 class="text-3xl font-bold text-gray-800" { "Styled heading" }
        p class="text-gray-500 mt-2" { "A paragraph with Tailwind styles." }
    }
}
```

Then build:

```bash
cargo build
```

The build script compiles `static/css/input.css` into `static/css/autumn.css`,
served at `/static/css/autumn.css` (reference it with
`asset_url("css/autumn.css")`). The compiled file is a build artifact and is
gitignored.

Your `input.css` starts with the Tailwind v4 import:

```css
@import "tailwindcss";
```

Add custom CSS below it. Component CSS for Autumn's built-in widgets ships from
the framework itself at `autumn_web::ui::WIDGETS_CSS_PATH`, so `input.css` only
needs your app's own styles.

> **Skipping Tailwind:** the build script treats the Tailwind binary as
> optional — without it, `cargo build` simply skips the CSS step and the app
> runs unstyled. To drop Tailwind for good, delete `tailwind.config.js` and
> `static/css/input.css` and remove the Tailwind branch from `build.rs`. Keep
> the rest of `build.rs`: it also bakes the git SHA, branch, and build
> timestamp into the binary for `/actuator/info`.

---

## Add interactivity with htmx

Autumn vendors [htmx](https://htmx.org/) into `static/js/` with a pinned
subresource-integrity hash and serves it through
`javascript_include_tag("htmx")`. Include that in your layout, then use htmx
attributes in your templates.

The handlers below mutate data, so they need the `csrf-token` meta tag and the
`HTMX_CSRF_JS_PATH` helper from the [layout above](#render-html-with-maud). If
you skipped that, add it now — otherwise these buttons will work on your
machine and return `403` in production.

Here is a toggle-and-delete pair that updates a todo without a full page
reload:

```rust
use autumn_web::prelude::*;
use autumn_web::extract::Path;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::models::Todo;
use crate::schema::todos;

/// Render a single todo item with htmx controls.
fn todo_item(todo: &Todo) -> Markup {
    let title_class = if todo.completed {
        "line-through text-gray-400"
    } else {
        "text-gray-800"
    };

    html! {
        li id=(format!("todo-{}", todo.id))
           class="flex items-center gap-3 p-3 bg-white rounded shadow" {
            // Toggle — POST via htmx, swap this <li> with the response.
            button hx-post=(format!("/todos/{}/toggle", todo.id))
                   hx-target=(format!("#todo-{}", todo.id))
                   hx-swap="outerHTML" {
                @if todo.completed { "\u{2713}" } @else { "\u{25CB}" }
            }
            span class=(title_class) { (todo.title) }
            // Delete — returns an empty body, htmx removes the element.
            button hx-delete=(format!("/todos/{}", todo.id))
                   hx-target=(format!("#todo-{}", todo.id))
                   hx-swap="outerHTML" {
                "\u{00D7}"
            }
        }
    }
}

/// Toggle completion — returns the updated HTML fragment.
#[post("/todos/{id}/toggle")]
#[public]
async fn toggle(id: Path<i64>, mut db: Db) -> AutumnResult<Markup> {
    let updated: Todo = diesel::update(todos::table.find(*id))
        .set(todos::completed.eq(diesel::dsl::not(todos::completed)))
        .returning(Todo::as_returning())
        .get_result(&mut *db)
        .await
        .map_err(AutumnError::not_found)?;

    Ok(todo_item(&updated))
}

/// Delete a todo — empty response body, so htmx removes the element.
#[delete("/todos/{id}")]
#[public]
async fn delete_todo(id: Path<i64>, mut db: Db) -> AutumnResult<String> {
    diesel::delete(todos::table.find(*id))
        .execute(&mut *db)
        .await?;

    Ok(String::new())
}
```

The attributes you will reach for most:

| Attribute    | Purpose                                                    |
|--------------|------------------------------------------------------------|
| `hx-get`     | Issue a GET request to the URL                             |
| `hx-post`    | Issue a POST request to the URL                            |
| `hx-delete`  | Issue a DELETE request to the URL                          |
| `hx-target`  | CSS selector for the element to update                     |
| `hx-swap`    | How to insert the response (`outerHTML`, `innerHTML`, `beforeend`, …) |
| `hx-trigger` | Event that fires the request (default: the natural event)  |

The pattern: your handler returns an HTML fragment, htmx swaps it into the DOM.
No JavaScript required.

### Forms without JavaScript

The same flows should work with JavaScript disabled. Submit a plain `<form>`
with a hidden `_method` field and Autumn rewrites the request to the declared
method **before route matching**, so your `#[put]`, `#[patch]`, and `#[delete]`
handlers stay semantically honest:

```rust,no_run
use autumn_web::form::method_input;
use autumn_web::prelude::*;
use autumn_web::security::CsrfToken;

#[get("/todos/{id}/edit")]
#[public]
async fn edit_form(id: Path<i64>, csrf: Option<CsrfToken>) -> Markup {
    html! {
        form method="post" action=(format!("/todos/{}", *id)) {
            (method_input("DELETE"))   // Autumn rewrites this POST to DELETE
            @if let Some(token) = csrf.as_ref() {
                input type="hidden" name="_csrf" value=(token.token());
            }
            button type="submit" { "Delete" }
        }
    }
}
```

The `_csrf` field is not optional dressing. CSRF still treats the transport
`POST` as unsafe, and the `prod` profile turns CSRF protection on by default —
so a form that omits the token gets a `403 Forbidden` before your `#[delete]`
handler ever runs, even though it works fine in `dev`.

Beyond that, the override is accepted only for same-origin, form-urlencoded
`POST` requests, and `autumn routes` keeps reporting the declared method so
route listings and OpenAPI stay accurate. If you build forms with
`ChangesetForm::form_tag`, the method and CSRF inputs are emitted for you. See
[nested forms](nested-forms.md) and [submit tokens](submit-tokens.md) for the
full form story.

---

## Error handling

`AutumnResult<T>` is `Result<T, AutumnError>`. Any handler that can fail should
return it.

### The `?` operator

Any `std::error::Error` converts to an `AutumnError` with HTTP 500:

```rust
#[get("/users")]
#[public]
async fn list_users(mut db: Db) -> AutumnResult<Json<Vec<User>>> {
    let users = users::table.load(&mut *db).await?; // 500 on failure
    Ok(Json(users))
}
```

### Status refinement

For expected failures, use a status constructor:

```rust
#[get("/users/{id}")]
#[public]
async fn get_user(id: Path<i64>, mut db: Db) -> AutumnResult<Json<User>> {
    let user = users::table
        .find(*id)
        .first(&mut *db)
        .await
        .map_err(AutumnError::not_found)?; // 404

    Ok(Json(user))
}
```

Each constructor has an `_msg` twin that takes a string instead of a source
error — `AutumnError::not_found_msg("no such todo")`.

| Constructor                              | HTTP status               |
|------------------------------------------|---------------------------|
| `AutumnError::bad_request(e)`            | 400 Bad Request           |
| `AutumnError::unauthorized(e)`           | 401 Unauthorized          |
| `AutumnError::forbidden(e)`              | 403 Forbidden             |
| `AutumnError::not_found(e)`              | 404 Not Found             |
| `AutumnError::conflict(e)`               | 409 Conflict              |
| `AutumnError::unprocessable(e)`          | 422 Unprocessable Entity  |
| `AutumnError::internal_server_error(e)`  | 500 Internal Server Error |
| `err.with_status(StatusCode::…)`         | Any status code           |

### The response shape

Autumn answers API clients with
[Problem Details](https://www.rfc-editor.org/rfc/rfc9457) (RFC 7807, obsoleted
by RFC 9457) bodies, served as `application/problem+json`:

```json
{
  "type": "https://autumn.dev/problems/not-found",
  "title": "Not Found",
  "status": 404,
  "detail": "Record not found",
  "instance": "/users/42",
  "code": "autumn.not_found",
  "request_id": "0f1c2d3e-4a5b-6c7d-8e9f-0a1b2c3d4e5f",
  "errors": []
}
```

`code` is a stable machine-readable identifier, `request_id` correlates with
your logs, and `errors` carries field-level validation failures when the error
came from a `Validate` rejection. Browsers negotiating `text/html` get the
error page instead of JSON. Server-error detail is never echoed to clients in
production.

When a 5xx does reach a user, you can record it as a replayable
[failure capsule](failure-capsules.md) — the request, the database traffic it
produced, and the outcome, in one file that `autumn replay` re-runs offline —
then convert it with `autumn capsule test` into a committed regression test so
the same bug can never come back unnoticed.

---

## Configuration

Autumn resolves configuration in five layers, each overriding the last:

1. **Framework defaults** — compiled in, zero-config start
2. **Profile smart defaults** — built-in `dev` / `prod` behavior
3. **`autumn.toml`** — project-level overrides
4. **`autumn-{profile}.toml`** — profile-specific overrides
5. **`AUTUMN_*` environment variables** — deployment overrides, highest priority

### `autumn.toml` reference

```toml
[server]
host = "127.0.0.1"           # default
port = 3000                  # default
shutdown_timeout_secs = 30   # default, seconds to drain in-flight requests
# max_concurrent_requests = 256 # unset by default; sheds excess with a 503

[server.timeouts]
# Per-request wall-clock deadline. The prod profile enables 30s automatically.
# Streaming responses are never interrupted; any route can override with
# `#[get("/slow", timeout_ms = 120000)]` or `timeout = "off"`.
# request_timeout_ms = 30000

[database]
primary_url = "postgres://user:pass@localhost:5432/my_app"
# url = "postgres://…"       # single-primary alias for primary_url
# replica_url = "postgres://user:pass@localhost:5433/my_app"
pool_size = 10               # default, max connections per role
# primary_pool_size = 10
# replica_pool_size = 5
replica_fallback = "fail_readiness"  # or "primary"
connect_timeout_secs = 5     # default
# auto_migrate = false       # dev auto-applies; other profiles opt in

[log]
level = "info"               # supports tracing filter syntax
format = "Auto"              # Auto | Pretty | Json

[health]
path = "/health"             # default
# enabled = true

[actuator]
sensitive = false            # prod default; dev smart defaults expose more
```

The generated `autumn.toml` carries commented blocks for telemetry, mail, and
Redis sessions too. The [cloud-native guide](cloud-native.md) covers the full
production surface.

### Environment variable overrides

Every config field can be overridden with an environment variable. The pattern
is `AUTUMN_SECTION__FIELD` — a double underscore separates section from field:

| Variable                                 | Overrides                       |
|------------------------------------------|---------------------------------|
| `AUTUMN_SERVER__PORT`                    | `server.port`                   |
| `AUTUMN_SERVER__HOST`                    | `server.host`                   |
| `AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS`   | `server.shutdown_timeout_secs`  |
| `AUTUMN_SERVER__MAX_CONCURRENT_REQUESTS` | `server.max_concurrent_requests`|
| `AUTUMN_DATABASE__URL`                   | `database.url`                  |
| `AUTUMN_DATABASE__PRIMARY_URL`           | `database.primary_url`          |
| `AUTUMN_DATABASE__REPLICA_URL`           | `database.replica_url`          |
| `AUTUMN_DATABASE__POOL_SIZE`             | `database.pool_size`            |
| `AUTUMN_DATABASE__REPLICA_FALLBACK`      | `database.replica_fallback`     |
| `AUTUMN_DATABASE__AUTO_MIGRATE`          | `database.auto_migrate`         |
| `AUTUMN_LOG__LEVEL`                      | `log.level`                     |
| `AUTUMN_LOG__FORMAT`                     | `log.format`                    |
| `AUTUMN_HEALTH__PATH`                    | `health.path`                   |
| `AUTUMN_SECURITY__SIGNING_SECRET`        | `security.signing_secret`       |
| `AUTUMN_ENV`                             | active profile                  |

Profiles resolve in this order:

1. `AUTUMN_ENV`
2. `AUTUMN_PROFILE` (legacy alias)
3. `--profile <name>`
4. Build-mode auto-detection — `dev` for debug builds, `prod` for release

So you can keep shared defaults in `autumn.toml`, local settings in
`autumn-dev.toml`, and override the last few things in CI or deployment with
environment variables. Profile selectors are deliberately excluded from `.env`,
so a `.env` file can never switch the active profile.

### Log format behavior

| Format   | Behavior                                                 |
|----------|----------------------------------------------------------|
| `Auto`   | Pretty in development, JSON when the profile is production |
| `Pretty` | Always human-readable, colorized                         |
| `Json`   | Always structured JSON                                   |

### Running without a database

Omit the `[database]` section (or leave both `primary_url` and `url` unset) and
Autumn starts with no pool. Handlers that use `Db` return 503 Service
Unavailable. That is useful for static sites, database-free APIs, and early
development.

### Tuning the Tokio runtime

`#[autumn_web::main]` owns the `tokio::runtime::Builder` call, so the knobs you
would otherwise abandon the macro to reach are attribute arguments. All of them
are optional; with none, the runtime is
`Builder::new_multi_thread().enable_all()` — tokio's own defaults, which is
what most apps should keep.

| Argument | Value | `tokio::runtime::Builder` call |
|----------|-------|--------------------------------|
| `flavor` | `"multi_thread"` (default) or `"current_thread"` | picks the constructor |
| `worker_threads` | `usize` expression | `worker_threads` (multi-thread only) |
| `max_blocking_threads` | `usize` expression | `max_blocking_threads` |
| `thread_name` | `Into<String>` expression | `thread_name` |
| `thread_stack_size` | `usize` expression, in bytes | `thread_stack_size` |
| `thread_keep_alive` | duration string, e.g. `"30s"` | `thread_keep_alive` |
| `configure` | path to `fn(&mut Builder)` | runs last, after everything above |

```rust,no_run
use autumn_web::prelude::*;

#[get("/")]
#[public]
async fn index() -> &'static str { "ok" }

#[autumn_web::main(
    worker_threads = 4,
    max_blocking_threads = 64,
    thread_name = "autumn-worker",
    thread_keep_alive = "30s"
)]
async fn main() {
    autumn_web::app().routes(routes![index]).run().await;
}
```

The numeric arguments take arbitrary expressions, not only literals, so a
worker count can be computed at startup:

```rust,ignore
#[autumn_web::main(worker_threads = std::thread::available_parallelism().map_or(4, |n| n.get()))]
```

`configure` is the escape hatch for `Builder` methods the table does not name —
`on_thread_start`, `on_thread_stop`, `global_queue_interval`, and the rest. It
names a function taking `&mut tokio::runtime::Builder`, and it runs *after* the
declarative arguments, so it can also override them:

```rust,no_run
use autumn_web::prelude::*;
use autumn_web::reexports::tokio;

#[get("/")]
#[public]
async fn index() -> &'static str { "ok" }

fn tune_runtime(builder: &mut tokio::runtime::Builder) {
    builder.on_thread_start(|| eprintln!("runtime thread started"));
}

#[autumn_web::main(configure = tune_runtime)]
async fn main() {
    autumn_web::app().routes(routes![index]).run().await;
}
```

A typo'd argument, a repeated one, a literal `0` thread count, or a
`worker_threads` paired with `flavor = "current_thread"` (where it would do
nothing) are all compile errors rather than knobs that silently fail to apply.

Reach for these only with a measurement in hand. Autumn's own background work —
the job runner, scheduled tasks, the mailer — shares this runtime, so a worker
count set below what the machine offers throttles those alongside your
handlers.

### Escape hatch: mounting raw Axum routers

Prefer the route macros — you keep Autumn's discovery conventions and the
codebase stays uniform. When you need Axum-native composition (mounting a
third-party router such as GraphQL), use `.merge()` or `.nest()`:

```rust,no_run
use autumn_web::prelude::*;
use autumn_web::AppState;

#[get("/")]
#[public]
async fn index() -> &'static str { "ok" }

#[autumn_web::main]
async fn main() {
    let graphql = axum::Router::<AppState>::new()
        .route("/graphql", axum::routing::get(|| async { "graphql endpoint" }));

    autumn_web::app()
        .routes(routes![index])   // Autumn-managed routes
        .merge(graphql)           // raw Axum routes on the same app
        .run()
        .await;
}
```

Use `.merge()` for direct mounting and `.nest("/prefix", router)` to put every
route under a prefix. Merged and nested routers share the same `AppState` and
still pass through Autumn's global middleware, including `X-Request-Id`
response headers.

### Route collision diagnostics

Autumn preflights route registration and refuses to start when it can prove a
collision — a structured `RouterBuildError` naming the offending handlers
**before any router is mounted**, rather than an Axum panic mid-boot:

- **`FrameworkRouteOverlap`** — a user route lands on a path a framework route
  already owns (probes, actuator, dev live-reload).
- **`OpenApiPathCollision`** (feature `openapi`) — an `openapi_json_path` or
  `swagger_ui_path` collides with a `GET` route Autumn already owns.
- **`DuplicateUserRoute`** — two registered routes resolve to the same
  `(method, path)` after `.scoped()` prefix resolution. Distinct methods on the
  same exact path (`GET /admin` + `POST /admin`) are fine — Axum merges those
  into one `MethodRouter`.
- **`ConflictingRouteShape`** — two *different* path templates that Axum's
  matcher cannot tell apart, such as `/users/{id}` and `/users/{slug}`.
  Detection is delegated to **matchit**, the exact routing engine Axum 0.8
  uses, so the preflight mirrors Axum's real accept/reject behavior. Because
  matchit rejects these before method merging, they conflict regardless of HTTP
  method.

Each error names both handlers and both templates:

```text
conflicting route shapes: "show_user" ("/users/{id}") and "create_user"
("/users/{slug}") resolve to the same Axum path shape but use different path
templates; axum's matchit router rejects this as a route conflict regardless
of HTTP method — rename the captures so both use the same template, or make
their static paths distinct
```

Routers registered through `.merge()` or `.nest()` cannot be introspected
through Axum's public API, so a collision inside one still surfaces as a
startup panic. The preflight emits a `tracing::warn!` ("check skipped") in that
case — keep raw merged routers on paths disjoint from your managed routes.

---

## Test what you built

The scaffold ships working tests in `tests/integration_test.rs`. They boot the
full Autumn middleware pipeline in-process — security headers, routing,
tracing, request IDs — without binding a TCP listener, and need no Docker:

```rust
use autumn_web::prelude::*;
use autumn_web::test::TestApp;

#[tokio::test]
async fn get_index_returns_200() {
    let client = TestApp::new().routes(routes![index]).build();

    client
        .get("/")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Welcome");
}
```

```bash
cargo test
```

For database-backed tests, `autumn test` resolves a `*_test` database URL,
creates it, migrates it, and then shells out to `cargo test` with
`AUTUMN_ENV=test` exported — refusing to run against a non-test database name:

```bash
autumn test
autumn test --reset   # drop and recreate first, for schema drift
```

See the [testing guide](testing.md) for `TestDb`, fixtures, and
[system tests](system-tests.md) for browser-driven coverage.

---

## Before you deploy

The generated app starts with local-safe defaults: in-memory sessions,
in-process `#[scheduled]` tasks, an ephemeral signing secret, and a generic
container Dockerfile. Before running multiple replicas you usually want to:

1. Set `AUTUMN_ENV=prod`
2. Set a durable `AUTUMN_SECURITY__SIGNING_SECRET` and a trusted-hosts list
3. Wire `/live`, `/ready`, and `/startup` into your platform's probes
4. Enable OTLP telemetry and point it at your collector
5. Move sessions to Redis
6. Run migrations as a one-shot job before starting web replicas

`autumn doctor --strict` checks most of that for you, and
`autumn release init --target <platform>` scaffolds deployment assets for
Docker Compose, Fly, AWS, GCP, and Azure. The
[cloud-native guide](cloud-native.md) and [deployment guide](deployment.md)
carry the full story.

---

## What's next

You now have an Autumn application with routes, database access, HTML
rendering, styling, interactivity, tests, health checks, and actuator
endpoints. Where to go from here:

**Go deeper on the basics**
- [Tutorial](tutorial/index.md) — the same ground at book length, with checkpoints
- [Code generators](generators.md) — the full `autumn generate` surface
- [Repositories](repositories.md) — typed data access, bulk CRUD, hooks
- [Migrations](migrations.md) and [declarative schema](declarative-schema.md)
- [Testing](testing.md) — `TestApp`, `TestDb`, fixtures

**Add capabilities**
- [Authentication](authentication.md) and [authorization](authorization.md)
- [Background jobs](jobs.md) and [scheduled tasks](tasks.md)
- [Real-time](realtime.md) — SSE, WebSockets, presence
- [Mail](mail.md), [storage](storage.md), [search](search.md), [i18n](i18n.md)
- [OpenAPI](openapi.md) — generated docs and a Swagger UI

**Operate it**
- [Cloud-native](cloud-native.md) and [deployment](deployment.md)
- [Metrics](metrics.md), [operator alerts](operator-alerts.md), [audit logging](audit-logging.md)
- [Failure capsules](failure-capsules.md) — record and replay production 500s

**Reference**
- [What happens when](what-happens-when.md) — edge cases and failure modes
- [Macro transparency](macro-transparency.md) — exactly what each macro expands to
- [Coming from other frameworks](coming-from-other-frameworks.md) — Rails, Spring Boot, Django, Laravel mappings
- Example apps: [`examples/todo-app`](../../examples/todo-app),
  [`examples/blog`](../../examples/blog),
  [`examples/bookmarks`](../../examples/bookmarks),
  [`examples/wiki`](../../examples/wiki),
  [`examples/reddit-clone`](../../examples/reddit-clone)

Autumn is pre-1.0 and moving quickly. File issues, reach for the Axum escape
hatches when you need them, and ship something.
