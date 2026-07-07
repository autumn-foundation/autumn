# Getting Started with Autumn

This guide takes you from zero to a running Autumn web app with routes, a
database, HTML templates, interactive UI, and the published `autumn-web` 0.4
release line. Budget about 30 minutes.

Autumn is a convention-over-configuration web framework for Rust, built on
[Axum](https://github.com/tokio-rs/axum). It bundles Diesel (database), Maud
(HTML), Tailwind CSS (styling), htmx (interactivity), actuator endpoints,
profile-aware configuration, and newer CLI workflows behind a Spring Boot-style
developer experience.

> **Version note:** This is the published-user path for `autumn-web` 0.4.x and
> `autumn-cli` 0.4.x as of 2026-05-11. If you are contributing from a source
> checkout, use the local development commands below only after confirming the
> workspace version still matches this guide.

---

## Prerequisites

- **Rust 1.88.0+** (edition 2024) -- install via [rustup](https://rustup.rs/)
- **PostgreSQL** -- only needed if you want database features; Autumn runs
  fine without one
- A terminal and a text editor

Verify your Rust toolchain:

```bash
rustc --version   # 1.88.0 or later
cargo --version
```

---

## Install the CLI

Autumn ships a small CLI for project scaffolding and tooling setup. Install the
published CLI from crates.io:

```bash
cargo install autumn-cli --version 0.6.0
```

For local development only, from an Autumn source checkout, install the CLI you
just built instead:

```bash
cargo install --path autumn-cli
```

This gives you the `autumn` binary with the core workflow commands:

| Command          | What it does                                |
|------------------|---------------------------------------------|
| `autumn doctor`  | Diagnose your environment before first run  |
| `autumn new`     | Scaffold a new project                      |
| `autumn setup`   | Download Tailwind CSS (with checksum verify) |
| `autumn dev`     | Run the dev server with file watching        |
| `autumn build`   | Pre-render `#[static_get]` routes into `dist/` |
| `autumn migrate` | Run migrations or inspect migration status   |
| `autumn seed`    | Populate the database with representative data |

---

## Run the Doctor

After installing the CLI, the first command to run from any Autumn project
root is `autumn doctor`. It checks your environment for common first-run
problems and tells you exactly what to fix before you waste time chasing
cryptic errors:

```bash
autumn doctor
```

Sample output on a healthy system:

```
🍂 autumn doctor

✅ rust_toolchain — rustc 1.88.0 ≥ MSRV 1.88.0
✅ version_compat — autumn-cli 0.5.0 matches autumn-web 0.5.0
✅ autumn_toml — autumn.toml is valid
✅ db_connectivity — Postgres reachable at localhost:5432
✅ pending_migrations — no pending migrations
✅ port_bindable — port 3000 is available
✅ tailwind_binary — target/autumn/tailwindcss is present
✅ stale_artifacts — artifacts look fresh

8 passed, 0 warnings, 0 failed — all clear
```

If anything is wrong, `autumn doctor` prints a one-line remediation hint
beneath the failing check.

**Exit codes**: `0` when all checks pass (warnings are allowed); `1` when any
check fails. Use `--strict` to treat warnings as failures (useful in CI).
Use `--json` for machine-readable output:

```bash
# CI pre-flight gate (fail on warnings too)
autumn doctor --strict

# Machine-readable output for scripts
autumn doctor --json
```

---

## Create a Project

```bash
autumn new my-app
cd my-app
```

This generates:

```
my-app/
  Cargo.toml
  autumn.toml          # framework configuration
  Dockerfile           # production container image
  .dockerignore
  build.rs             # Tailwind CSS build pipeline
  src/
    main.rs            # your application entry point
  static/
    css/
      input.css        # Tailwind directives
  tailwind.config.js
  migrations/          # Diesel migrations (empty for now)
  .gitignore
```

---

## Project Structure

The files that matter right now:

| File                | Purpose                                    |
|---------------------|--------------------------------------------|
| `src/main.rs`       | Routes and application bootstrap           |
| `autumn.toml`       | Server, probes, telemetry, database config |
| `Dockerfile`        | Multi-stage production image scaffold      |
| `.dockerignore`     | Keeps local junk out of container builds   |
| `build.rs`          | Compiles Tailwind CSS on `cargo build`     |
| `static/`           | Auto-served at `/static/` (CSS, JS, images)|
| `migrations/`       | Diesel SQL migrations                      |

---

## Your First Route

Open `src/main.rs`. The scaffolded app includes a Maud layout, embedded
migrations, and these starter routes:

```rust
#[get("/")]
async fn index() -> maud::Markup {
    layout("Welcome", maud::html! {
        h1 { "Welcome to my-app!" }
        p { "Edit " code { "src/main.rs" } " to get started." }
    })
}

#[get("/hello")]
async fn hello() -> &'static str {
    "Hello, Autumn!"
}

#[get("/hello/{name}")]
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

The key pieces:

- **`#[get("/path")]`** -- annotates a handler for GET requests. Also
  available: `#[post]`, `#[put]`, `#[delete]`.
- **`routes![...]`** -- collects annotated handlers into a `Vec<Route>`.
- **`autumn_web::app().routes(...).run().await`** -- the app builder. Load config,
  create the database pool, mount routes, start the server.
- **`.migrations(MIGRATIONS)`** -- embeds and applies the app's Diesel
  migrations when a database is configured.
- **`#[autumn_web::main]`** -- sets up the Tokio async runtime. It is a thin
  wrapper around `#[tokio::main]`.

Handlers are regular async functions. They can return any type that Axum can
turn into an HTTP response: `&str`, `String`, `Json<T>`, `Markup` (Maud HTML),
or your own `impl IntoResponse`.

---

## Run It

```bash
autumn dev
```

If you prefer not to use watch mode:

```bash
cargo run
```

You will see log output like:

```
  INFO autumn: Database not configured
  INFO autumn: Listening addr=127.0.0.1:3000
```

Visit [http://localhost:3000](http://localhost:3000) -- you should see
"Welcome to my-app!". Try
[http://localhost:3000/hello/world](http://localhost:3000/hello/world) for the
path parameter route.

A health check is automatically mounted at
[http://localhost:3000/health](http://localhost:3000/health). Actuator
endpoints are also auto-mounted at
[http://localhost:3000/actuator/health](http://localhost:3000/actuator/health),
[http://localhost:3000/actuator/info](http://localhost:3000/actuator/info), and
[http://localhost:3000/actuator/metrics](http://localhost:3000/actuator/metrics).
Probe endpoints are also available at `/live`, `/ready`, and `/startup`.

The `/health` response looks like:

```json
{ "status": "ok", "version": "0.6.0" }
```

Press **Ctrl+C** to stop the server (graceful shutdown with a configurable
drain timeout).

---

## Production Notes

The generated app starts with local-safe defaults:

- sessions are in-memory unless you switch to Redis
- `#[scheduled]` tasks run in-process
- the generated Dockerfile is generic container scaffolding, not a full Kubernetes deployment

Before deploying multiple replicas, you should usually:

1. Set `AUTUMN_PROFILE=prod`
2. Configure `/live`, `/ready`, and `/startup` in your platform probes
3. Enable OTLP telemetry and point it at your collector
4. Move sessions to Redis
5. Run migrations as a one-shot job before starting web replicas

The scaffolded `Dockerfile` and `autumn.toml` include commented examples for
probes, telemetry, and Redis sessions. For the full deployment story, read the
[Cloud-Native Guide](cloud-native.md).

---

## Path Parameters

Axum-style path parameters use curly braces in the route pattern and the
`Path<T>` extractor in the handler signature:

```rust
use autumn_web::extract::Path;
use autumn_web::get;

#[get("/users/{id}")]
async fn get_user(id: Path<i32>) -> String {
    format!("User #{}", *id)
}
```

Multiple parameters work by destructuring a tuple:

```rust
#[get("/orgs/{org}/repos/{repo}")]
async fn get_repo(Path((org, repo)): Path<(String, String)>) -> String {
    format!("{org}/{repo}")
}
```

You can also use `Query<T>` for query string parameters:

```rust
use autumn_web::extract::Query;
use serde::Deserialize;

#[derive(Deserialize)]
struct Pagination {
    page: Option<u32>,
    per_page: Option<u32>,
}

#[get("/items")]
async fn list_items(Query(params): Query<Pagination>) -> String {
    let page = params.page.unwrap_or(1);
    let per_page = params.per_page.unwrap_or(20);
    format!("Page {page}, showing {per_page} items")
}
```

---

## Set Up the Database

Autumn uses [Diesel](https://diesel.rs/) with
[diesel-async](https://github.com/weiznich/diesel_async) and
[deadpool](https://docs.rs/deadpool) for async Postgres connections. Autumn
drives Diesel for you — the steps below stand up and migrate the database using
`autumn` commands end to end.

### 1. Install the Diesel CLI

`autumn migrate` shells out to the Diesel CLI to apply migrations, so install it
once:

```bash
cargo install diesel_cli --no-default-features --features postgres
```

### 2. Configure the connection

Edit `autumn.toml` and uncomment the `[database]` section:

```toml
[database]
url = "postgres://localhost/my_app"
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
auto_migrate_in_production = false
```

`Db`, transactions, advisory locks, and `autumn migrate` use the primary role.
The optional replica role is for read paths that can tolerate replica replay
lag according to `replica_fallback`.

You can also set the primary URL via environment variable, which takes
precedence over the TOML file:

```bash
export AUTUMN_DATABASE__PRIMARY_URL="postgres://localhost/my_app"
```

(Note the double underscore `__` separating section from field.)

### 3. Create the database

```bash
autumn db create
```

`autumn db create` reads the connection you just configured and creates the
database on its server. It is idempotent — run it again and it simply reports
that the database already exists. (Need a clean slate while iterating on your
schema? See `autumn db reset` below.)

### 4. Create a migration

```bash
autumn generate migration CreateTodos
```

This emits a timestamped migration directory under `migrations/` with `up.sql`
and `down.sql` files. Edit the generated `up.sql`:

```sql
CREATE TABLE todos (
    id SERIAL PRIMARY KEY,
    title TEXT NOT NULL,
    completed BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);
```

And the `down.sql`:

```sql
DROP TABLE todos;
```

Run it:

```bash
autumn migrate
```

`autumn migrate` applies every pending migration to the primary database and
regenerates `src/schema.rs` with Diesel's table macro.

> **Tip — reset the dev database.** While iterating on your schema, run
> `autumn db reset` to drop, recreate, migrate, and (when a `src/bin/seed.rs`
> exists) seed the database in a single step. It refuses to run against a
> production profile unless you pass `--force`.

---

## Define a Model

Create `src/models.rs`:

```rust
use diesel::prelude::*;
use serde::{Deserialize, Serialize};

use crate::schema::todos;

#[derive(Queryable, Selectable, Serialize)]
#[diesel(table_name = todos)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Todo {
    pub id: i32,
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

Autumn also provides a `#[model]` attribute macro that auto-derives the
Diesel and Serde traits for you:

```rust
use crate::schema::todos;

// Equivalent to the manual derives above (Queryable, Selectable,
// Insertable, Serialize, Deserialize) plus #[diesel(table_name = todos)]
#[autumn_web::model(table = "todos")]
pub struct Todo {
    pub id: i32,
    pub title: String,
    pub completed: bool,
    pub created_at: chrono::NaiveDateTime,
}
```

If you omit the `table = "..."` argument, the table name is inferred from the
struct name: `BlogPost` becomes `blog_posts`, `User` becomes `users`.

Add the required dependencies to `Cargo.toml`:

```toml
[dependencies]
autumn-web = "0.6"
chrono = { version = "0.4", features = ["serde"] }
diesel = { version = "2", features = ["postgres", "chrono"] }
diesel-async = { version = "0.8", features = ["postgres"] }
serde = { version = "1", features = ["derive"] }
```

Don't forget to declare the modules in `main.rs`:

```rust
mod models;
mod schema;
```

---

## Query the Database

Use the `Db` extractor to get an async Postgres connection from the pool.
It implements `Deref<Target = AsyncPgConnection>`, so you can pass `&mut *db`
directly to Diesel queries:

```rust
use autumn_web::prelude::*;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::models::{NewTodo, Todo};
use crate::schema::todos;

#[get("/todos")]
async fn list_todos(mut db: Db) -> AutumnResult<Json<Vec<Todo>>> {
    let all_todos = todos::table
        .order(todos::created_at.desc())
        .select(Todo::as_select())
        .load(&mut *db)
        .await?;

    Ok(Json(all_todos))
}

#[post("/api/todos")]
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

- **`Db`** is an extractor, not a global. Declare it in your handler signature
  and Autumn hands you a pooled connection. The connection returns to the pool
  when the handler completes.
- **`AutumnResult<T>`** is `Result<T, AutumnError>`. The `?` operator
  converts any `std::error::Error` into an `AutumnError` with status 500.
  Diesel errors, I/O errors, serde errors -- they all "just work" with `?`.
- **`mut db: Db`** -- you need `mut` because Diesel queries take `&mut` on
  the connection.

Register the new handlers in your `main()`:

```rust
mod models;
mod schema;

use autumn_web::routes;

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![list_todos, create_todo])
        .run()
        .await;
}
```

Test it:

```bash
# Create a todo
curl -X POST http://localhost:3000/api/todos \
  -H "Content-Type: application/json" \
  -d '{"title": "Write Autumn guide"}'

# List todos
curl http://localhost:3000/todos
```

---

## Render HTML with Maud

Autumn re-exports [Maud](https://maud.lambda.xyz/), a compile-time HTML
templating library. Return `Markup` from a handler to send HTML:

```rust
use autumn_web::prelude::*;

#[get("/")]
async fn index() -> Markup {
    html! {
        (autumn_web::PreEscaped("<!DOCTYPE html>"))
        html lang="en" {
            head {
                meta charset="utf-8";
                title { "My App" }
                link rel="stylesheet" href="/static/css/autumn.css";
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

| Maud                                  | HTML output                              |
|---------------------------------------|------------------------------------------|
| `h1 { "Hello" }`                     | `<h1>Hello</h1>`                         |
| `div class="box" { "content" }`      | `<div class="box">content</div>`         |
| `input type="text" name="q";`        | `<input type="text" name="q">`           |
| `(variable)`                          | Escaped interpolation                    |
| `(PreEscaped(raw_html))`             | Unescaped interpolation                  |
| `@if cond { ... } @else { ... }`     | Conditional rendering                   |
| `@for item in &items { ... }`        | Loop rendering                           |

Extract reusable layouts into functions:

```rust
fn layout(title: &str, content: Markup) -> Markup {
    html! {
        (autumn_web::PreEscaped("<!DOCTYPE html>"))
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                link rel="stylesheet" href="/static/css/autumn.css";
                script src="/static/js/htmx.min.js" {}
            }
            body class="bg-gray-100 min-h-screen" {
                div class="max-w-2xl mx-auto py-10 px-4" {
                    (content)
                }
            }
        }
    }
}
```

Then use it in handlers:

```rust
#[get("/about")]
async fn about() -> Markup {
    layout("About", html! {
        h1 { "About this app" }
        p { "Built with Autumn, Maud, and Tailwind." }
    })
}
```

---

## Style with Tailwind CSS

Autumn integrates [Tailwind CSS](https://tailwindcss.com/) via a `build.rs`
that runs the Tailwind standalone CLI at compile time.

### 1. Download Tailwind

```bash
autumn setup
```

This downloads the platform-specific Tailwind CSS binary to
`target/autumn/tailwindcss` with SHA-256 checksum verification. Use
`autumn setup --force` to re-download.

### 2. Write Tailwind classes in your templates

The `build.rs` scans `src/**/*.rs` for class names, so Tailwind utility
classes inside Maud templates are automatically picked up:

```rust
html! {
    div class="max-w-2xl mx-auto py-10 px-4" {
        h1 class="text-3xl font-bold text-gray-800" { "Styled heading" }
        p class="text-gray-500 mt-2" { "A paragraph with Tailwind styles." }
    }
}
```

### 3. Build

```bash
cargo build
```

The build script compiles `static/css/input.css` (which contains Tailwind
directives) into `static/css/autumn.css`. This file is auto-served at
`/static/css/autumn.css`.

Your `input.css` starts with:

```css
@tailwind base;
@tailwind components;
@tailwind utilities;
```

Add custom CSS below the directives as needed.

> **Skipping Tailwind:** If you do not need CSS, delete `build.rs` and the
> Tailwind-related files. Autumn runs fine without them.

---

## Add Interactivity with htmx

Autumn bundles [htmx](https://htmx.org/) and auto-serves it at
`/static/js/htmx.min.js`. Include the script tag in your layout, then use
htmx attributes in your Maud templates.

Here is a toggle button that updates a todo without a full page reload:

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
            // Toggle button -- POST via htmx, swap this <li> with the response
            button hx-post=(format!("/todos/{}/toggle", todo.id))
                   hx-target=(format!("#todo-{}", todo.id))
                   hx-swap="outerHTML" {
                @if todo.completed { "\u{2713}" } @else { "\u{25CB}" }
            }
            span class=(title_class) { (todo.title) }
            // Delete button -- returns empty string, htmx removes the element
            button hx-delete=(format!("/todos/{}", todo.id))
                   hx-target=(format!("#todo-{}", todo.id))
                   hx-swap="outerHTML" {
                "\u{00D7}"
            }
        }
    }
}

/// Toggle completion status -- returns the updated HTML fragment.
#[post("/todos/{id}/toggle")]
async fn toggle(id: Path<i32>, mut db: Db) -> AutumnResult<Markup> {
    let todo: Todo = todos::table
        .find(*id)
        .select(Todo::as_select())
        .first(&mut *db)
        .await
        .map_err(AutumnError::not_found)?;

    diesel::update(todos::table.find(*id))
        .set(todos::completed.eq(!todo.completed))
        .execute(&mut *db)
        .await?;

    let updated: Todo = todos::table
        .find(*id)
        .select(Todo::as_select())
        .first(&mut *db)
        .await
        .map_err(AutumnError::not_found)?;

    Ok(todo_item(&updated))
}

/// Delete a todo -- returns empty string so htmx removes the element.
#[delete("/todos/{id}")]
async fn delete_todo(id: Path<i32>, mut db: Db) -> AutumnResult<String> {
    diesel::delete(todos::table.find(*id))
        .execute(&mut *db)
        .await?;

    Ok(String::new())
}
```

The htmx attributes you will use most often:

| Attribute     | Purpose                                              |
|---------------|------------------------------------------------------|
| `hx-get`      | Issue a GET request to the URL                       |
| `hx-post`     | Issue a POST request to the URL                      |
| `hx-delete`   | Issue a DELETE request to the URL                    |
| `hx-target`   | CSS selector for the element to update               |
| `hx-swap`     | How to insert the response (`outerHTML`, `innerHTML`, `beforeend`, etc.) |
| `hx-trigger`  | Event that triggers the request (default: natural event) |

The pattern: your handler returns an HTML fragment (not a full page), htmx
swaps it into the DOM. No JavaScript required.

### Plain HTML forms targeting PUT/PATCH/DELETE

When you want the same edit and delete flows to work with JavaScript
disabled (or before htmx loads), submit a plain `<form>` and include a
hidden `_method` field. Autumn rewrites the request to the declared
HTTP method **before route matching**, so your `#[put]`, `#[patch]`, and
`#[delete]` handlers stay semantically honest:

```rust,no_run
use autumn_web::form::method_input;
use autumn_web::prelude::*;
use autumn_web::security::CsrfToken;

#[get("/todos/{id}/edit")]
async fn edit_form(id: Path<i32>, csrf: Option<CsrfToken>) -> Markup {
    html! {
        form method="post" action=(format!("/todos/{}", *id)) {
            // Hidden field. Autumn rewrites this POST to DELETE.
            (method_input("DELETE"))
            @if let Some(token) = csrf.as_ref() {
                input type="hidden" name="_csrf" value=(token.token());
            }
            button type="submit" { "Delete" }
        }
    }
}
```

A few notes:

- The override is honoured only for `POST` requests with
  `Content-Type: application/x-www-form-urlencoded`. Headers like
  `X-HTTP-Method-Override` are intentionally not enabled by default.
- It is also enforced as **same-origin**: the request must carry
  `Sec-Fetch-Site: same-origin` or `none` (sent by every browser
  since ~2020), or — for `same-site` or when `Sec-Fetch-Site` is
  absent — its `Origin` header must match the request's scheme,
  host, and port. This is stricter than `same-site` alone, because
  `same-site` accepts sibling subdomains under the same registrable
  domain (e.g. `evil.example.com` -> `app.example.com`). When
  running behind a TLS-terminating reverse proxy, the scheme is
  read from `X-Forwarded-Proto` (leftmost client-facing value);
  the host is read from `X-Forwarded-Host` if surfaced, otherwise
  from `Host`. Requests that don't meet these conditions are
  forwarded as the original `POST` so a cross-origin form can
  never reach a route declared only as `#[delete]`. The
  `autumn_web::test::TestApp` `form()` helper sets
  `Sec-Fetch-Site: same-origin` automatically.
- Unknown override values (anything other than `PUT`, `PATCH`, `DELETE`,
  case-insensitive) reject with `400 Bad Request` before your handler runs.
- Form-urlencoded bodies larger than 2 MiB are rejected with
  `413 Payload Too Large` so an oversized form with `_method=DELETE`
  isn't silently demoted to a `POST` (body-size-driven semantics).
  Use `multipart/form-data` for large submissions.
- CSRF still treats the transport `POST` as unsafe — an overridden
  `DELETE` without a valid token returns `403 Forbidden` just like any
  other mutating POST.
- `autumn routes` and `/actuator/routes` keep reporting the declared
  method (`DELETE`, `PUT`, `PATCH`) so route listings, OpenAPI docs, and
  log filters stay accurate.

If you build the form through `ChangesetForm::form_tag`, just pass the
declared method and the helper handles the override and CSRF inputs
for you:

```rust,ignore
form.form_tag("/todos/42", "delete", html! { button { "Delete" } })
// Renders: <form method="post" action="/todos/42">
//            <input type="hidden" name="_method" value="DELETE">
//            <input type="hidden" name="_csrf" value="...">
//            <button>Delete</button>
//          </form>
```

---

## Error Handling

`AutumnResult<T>` is `Result<T, AutumnError>`. Every handler that can fail
should return this type.

### The `?` operator

Any `std::error::Error` converts to `AutumnError` with HTTP 500 automatically:

```rust
#[get("/users")]
async fn list_users(mut db: Db) -> AutumnResult<Json<Vec<User>>> {
    let users = users::table.load(&mut *db).await?; // 500 on failure
    Ok(Json(users))
}
```

### Status refinement

For expected errors, use the status constructors:

```rust
#[get("/users/{id}")]
async fn get_user(id: Path<i32>, mut db: Db) -> AutumnResult<Json<User>> {
    let user = users::table
        .find(*id)
        .first(&mut *db)
        .await
        .map_err(AutumnError::not_found)?; // 404

    Ok(Json(user))
}
```

Available constructors:

| Method                      | HTTP Status                |
|-----------------------------|----------------------------|
| `AutumnError::not_found(e)` | 404 Not Found              |
| `AutumnError::bad_request(e)` | 400 Bad Request          |
| `AutumnError::unprocessable(e)` | 422 Unprocessable Entity |
| `err.with_status(StatusCode::FORBIDDEN)` | Any status code |

Error responses are JSON:

```json
{
  "error": {
    "status": 404,
    "message": "Record not found"
  }
}
```

---

## Configuration

Autumn uses a five-layer configuration system:

1. **Framework defaults** -- compiled into the binary, zero-config start
2. **Profile smart defaults** -- built-in `dev` / `prod` behavior
3. **`autumn.toml`** -- project-level overrides
4. **`autumn-{profile}.toml`** -- profile-specific overrides
5. **`AUTUMN_*` environment variables** -- deployment overrides (highest priority)

### `autumn.toml` reference

```toml
[server]
host = "127.0.0.1"          # default
port = 3000                  # default
shutdown_timeout_secs = 30   # default, seconds to drain in-flight requests

[database]
primary_url = "postgres://user:pass@localhost:5432/my_app"
# url = "postgres://user:pass@localhost:5432/my_app" # legacy single-primary alias
# replica_url = "postgres://user:pass@localhost:5433/my_app"
pool_size = 10               # default, max connections per role
# primary_pool_size = 10
# replica_pool_size = 5
replica_fallback = "fail_readiness" # or "primary"
connect_timeout_secs = 5     # default
auto_migrate_in_production = false

[log]
level = "info"               # default, supports tracing filter syntax
format = "Auto"              # Auto | Pretty | Json

[health]
path = "/health"             # default

[actuator]
sensitive = false            # prod default; dev smart defaults enable sensitive endpoints
```

### Environment variable overrides

Every config field can be overridden via environment variables. The pattern is
`AUTUMN_SECTION__FIELD` (double underscore separates section from field):

| Variable                             | Overrides              |
|--------------------------------------|------------------------|
| `AUTUMN_SERVER__PORT`                | `server.port`          |
| `AUTUMN_SERVER__HOST`                | `server.host`          |
| `AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS` | `server.shutdown_timeout_secs` |
| `AUTUMN_DATABASE__URL`               | `database.url`         |
| `AUTUMN_DATABASE__PRIMARY_URL`       | `database.primary_url` |
| `AUTUMN_DATABASE__REPLICA_URL`       | `database.replica_url` |
| `AUTUMN_DATABASE__POOL_SIZE`         | `database.pool_size`   |
| `AUTUMN_DATABASE__PRIMARY_POOL_SIZE` | `database.primary_pool_size` |
| `AUTUMN_DATABASE__REPLICA_POOL_SIZE` | `database.replica_pool_size` |
| `AUTUMN_DATABASE__REPLICA_FALLBACK`  | `database.replica_fallback` |
| `AUTUMN_DATABASE__CONNECT_TIMEOUT_SECS` | `database.connect_timeout_secs` |
| `AUTUMN_LOG__LEVEL`                  | `log.level`            |
| `AUTUMN_LOG__FORMAT`                 | `log.format`           |
| `AUTUMN_HEALTH__PATH`               | `health.path`          |
| `AUTUMN_PROFILE`                     | active profile         |

Profiles resolve in this order:

1. `AUTUMN_PROFILE`
2. `--profile <name>`
3. debug/release auto-detection (`dev` for debug, `prod` for release)

That means you can keep shared defaults in `autumn.toml`, put local dev
settings in `autumn-dev.toml`, and override the final few things in CI or
deployment with env vars.

### Log format behavior

| Format    | Behavior                                              |
|-----------|-------------------------------------------------------|
| `Auto`    | Pretty in development, JSON when `AUTUMN_ENV=production` |
| `Pretty`  | Always human-readable, colorized                     |
| `Json`    | Always structured JSON                                |

### Running without a database

If you omit the `[database]` section (or leave both `primary_url` and `url`
unset), Autumn starts without a database pool. Handlers that use `Db` will return 503 Service
Unavailable. This is useful for static sites, APIs that do not need a
database, or during early development.

### Escape hatch: mounting raw Axum routers

When route macros are enough, prefer them -- you keep Autumn's discovery
conventions and the codebase stays more uniform.

When you need Axum-native composition (for example, mounting a third-party
router like GraphQL), use `.merge()` or `.nest()`:

```rust,no_run
use autumn_web::prelude::*;
use autumn_web::AppState;

#[get("/")]
async fn index() -> &'static str { "ok" }

#[autumn_web::main]
async fn main() {
    let graphql = axum::Router::<AppState>::new()
        .route("/graphql", axum::routing::get(|| async { "graphql endpoint" }));

    autumn_web::app()
        .routes(routes![index]) // Autumn-managed routes
        .merge(graphql) // Raw Axum routes on the same app
        .run()
        .await;
}
```

Use `.merge()` for direct mounting and `.nest("/prefix", router)` when you want
all routes under a prefix (for example `/api/v2`).

Merged/nested routers share the same `AppState` and still pass through Autumn's
global middleware (including `X-Request-Id` response headers). Avoid defining
the same method+path in both managed and raw routers -- Axum treats overlaps as
an error during router construction.

---

## What's Next?

You now have a working Autumn application with routes, database access,
HTML rendering, Tailwind styling, htmx interactivity, health checks, and
actuator endpoints.

Here are some things to explore:

- **Organize routes into modules** -- call `.routes()` multiple times on the
  app builder to compose route groups:
  ```rust
  autumn_web::app()
      .routes(routes![index])
      .routes(routes![list_todos, create_todo, toggle, delete_todo])
      .run()
      .await;
  ```
- **Use `Form<T>`** for HTML form submissions instead of JSON:
  ```rust
  use autumn_web::extract::Form;

  #[post("/todos")]
  async fn create(mut db: Db, form: Form<NewTodo>) -> AutumnResult<Markup> {
      // form.0 is the deserialized NewTodo
      // ...
  }
  ```
- **Check `/health` and `/actuator/*`** -- `/health` gives a small health
  response, while actuator adds info, metrics, env/configprops, loggers, and
  scheduled task visibility depending on the active profile.
- **Localize your UI** -- enable the opt-in `i18n` feature flag on `autumn-web`
  and drop translation files at `i18n/<locale>.ftl`. See
  [the i18n guide](./i18n.md) for the convention, the `Locale` extractor, and
  the `t!()` macro.
- **Inspect request IDs** -- every response includes an `X-Request-Id` header
  (UUID v4) for log correlation.
- **Look at the example apps** -- [`examples/todo-app`](../../examples/todo-app),
  [`examples/blog`](../../examples/blog), [`examples/bookmarks`](../../examples/bookmarks),
  and [`examples/wiki`](../../examples/wiki) each exercise different parts of
  the current framework.

Autumn is still pre-1.0 and evolving quickly. File issues, break glass when you
need Axum escape hatches, and ship something.
