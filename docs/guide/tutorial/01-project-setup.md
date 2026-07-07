# Chapter 1: Project Setup

**Goal:** By the end of this chapter, you will have a new Autumn project that
compiles, runs, and responds to HTTP requests at `http://localhost:3000`.

---

## Prerequisites

Before you start, make sure you have:

- **Rust 1.88.0+** (edition 2024) — install from <https://rustup.rs> if you haven't already
- **The Autumn CLI** — install the published CLI with
  `cargo install autumn-cli --version 0.6.0`
- A terminal and a text editor

Docker is not needed until Chapter 3 (Database Setup). For now, you only need
Rust and the Autumn CLI.

If you are contributing from an Autumn source checkout, `cargo install --path
autumn-cli` is the local development only install path.

## Scaffold the Project

Run `autumn new` to generate a complete project skeleton:

```bash
autumn new todo-app
```

You will see output like this:

```
  Created todo-app/
  Created todo-app/Cargo.toml
  Created todo-app/autumn.toml
  Created todo-app/build.rs
  Created todo-app/src/main.rs
  Created todo-app/static/css/input.css
  Created todo-app/tailwind.config.js
  Created todo-app/.gitignore
  Created todo-app/migrations/

Get started:
  cd todo-app
  cargo run

Your app will be available at http://localhost:3000
```

Move into the project directory:

```bash
cd todo-app
```

Let's look at what was generated.

## Project Structure

```
todo-app/
+-- Cargo.toml
+-- autumn.toml
+-- build.rs
+-- src/
|   +-- main.rs
+-- static/
|   +-- css/
|       +-- input.css
+-- tailwind.config.js
+-- migrations/
|   +-- .gitkeep
+-- .gitignore
```

Each file has a specific role. Let's walk through them.

### `Cargo.toml`

```toml
[package]
name = "todo-app"
version = "0.1.0"
edition = "2024"

[dependencies]
autumn-web = "0.6"
```

This is a standard Rust project manifest. The only dependency is `autumn-web`
itself. Autumn re-exports everything you need (Axum, Maud, Tokio, etc.) so
you don't manage those crates directly.

### `src/main.rs`

The full generated file also defines the shared `layout(...)` helper and
embedded migration constant. The route core looks like this:

```rust
#[get("/")]
async fn index() -> maud::Markup {
    layout("Welcome", maud::html! {
        h1 { "Welcome to todo-app!" }
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

There is a lot happening in a small file. Here is what each piece does:

- **`#[get("/")]`** — a route macro that registers `index` as a GET handler
  for the root path. Autumn provides `#[get]`, `#[post]`, `#[put]`, and
  `#[delete]` macros.
- **`#[get("/hello/{name}")]`** — a route with a path parameter. The `{name}`
  segment is extracted into the handler's `Path<String>` argument.
- **`autumn_web::extract::Path`** — an extractor that pulls typed values from the
  URL path. Autumn re-exports Axum's extractors so you don't need `axum` as a
  direct dependency.
- **`#[autumn_web::main]`** — sets up the Tokio async runtime. This is equivalent
  to `#[tokio::main]` with Autumn's preferred configuration.
- **`autumn_web::app()`** — creates an application builder. You register routes
  with `.routes()` and start the server with `.run().await`.
- **`routes![index, hello, hello_name]`** — a macro that collects route
  handlers into a `Vec<Route>` for the app builder.
- **`.migrations(MIGRATIONS)`** — embeds the app's Diesel migrations so Autumn
  can apply them when a database is configured.

The pattern is always the same: define handlers with route macros, collect
them with `routes![]`, and pass them to `autumn_web::app().routes(...).run()`.

### `autumn.toml`

```toml
# Autumn configuration
# All values shown are defaults — uncomment and change as needed.

[server]
host = "127.0.0.1"
port = 3000
# shutdown_timeout_secs = 30

[log]
level = "info"
# format = "Auto"  # Auto | Pretty | Json

[health]
path = "/health"

# Uncomment to configure database:
# [database]
# url = "postgres://user:pass@localhost:5432/todo_app"
# primary_url = "postgres://user:pass@localhost:5432/todo_app"
# replica_url = "postgres://user:pass@localhost:5433/todo_app"
# pool_size = 10
# replica_fallback = "fail_readiness"
# connect_timeout_secs = 5
# auto_migrate_in_production = false
```

Autumn uses a five-layer configuration system:

1. **Framework defaults** — compiled into the binary (port 3000, log level
   info, etc.)
2. **Profile smart defaults** — built-in `dev` / `prod` behavior
3. **`autumn.toml`** — project-level overrides (this file)
4. **`autumn-{profile}.toml`** — profile-specific overrides
5. **`AUTUMN_*` environment variables** — deployment overrides (e.g.,
   `AUTUMN_SERVER__PORT=8080`)

Every value has a sensible default. You can delete `autumn.toml` entirely and
the app still runs. The file is generated with commented examples so you know
what is available.

The `[database]` section is commented out because you do not need a database
yet. You will uncomment it in Chapter 3.

### `build.rs`

```rust
fn main() {
    println!("cargo:rerun-if-changed=src/");
    println!("cargo:rerun-if-changed=tailwind.config.js");

    let tailwind = find_tailwind_cli();

    let status = std::process::Command::new(&tailwind)
        .args([
            "-i", "static/css/input.css",
            "-o", "static/css/autumn.css",
            "--content", "src/**/*.rs",
            "--minify",
        ])
        .status()
        .expect("Failed to run Tailwind CLI");

    if !status.success() {
        panic!("Tailwind CSS compilation failed");
    }
}
```

This build script runs the Tailwind CSS CLI at compile time. It scans your
Rust source files for Tailwind class names (they appear in Maud templates as
string literals), generates a minimal CSS file, and writes it to
`static/css/autumn.css`. The `cargo:rerun-if-changed` directives ensure it
only runs when source files or the Tailwind config change.

The `find_tailwind_cli()` function (omitted for brevity) looks for the
Tailwind binary in two places: `target/autumn/tailwindcss` (downloaded by
`autumn setup`) and then on your system `PATH`.

### `static/css/input.css`

```css
@tailwind base;
@tailwind components;
@tailwind utilities;
```

This is the Tailwind CSS entry point. The `@tailwind` directives are replaced
by Tailwind's generated styles during the build. You can add your own custom
CSS below these directives if needed.

### `tailwind.config.js`

```javascript
/** @type {import('tailwindcss').Config} */
module.exports = {
  content: ["src/**/*.rs"],
  theme: {
    extend: {},
  },
  plugins: [],
}
```

This tells Tailwind where to scan for class names. Since Maud templates live
in `.rs` files, the content glob is `src/**/*.rs`. Tailwind reads the string
literals in your Rust code and generates only the CSS classes you actually use.

### `.gitignore`

```gitignore
/target
static/css/autumn.css
```

The `/target` directory contains build artifacts. `static/css/autumn.css` is a
generated file (produced by `build.rs` from `input.css`) and should not be
committed.

### `migrations/`

An empty directory with a `.gitkeep` placeholder. You will add your first
database migration here in Chapter 3.

## Download the Tailwind CLI

The `build.rs` script needs the Tailwind CSS standalone CLI. Autumn provides a
command that downloads the correct binary for your platform:

```bash
autumn setup
```

This downloads the Tailwind CLI to `target/autumn/tailwindcss` (or
`tailwindcss.exe` on Windows) and verifies its SHA-256 checksum. The binary
is about 50 MB.

If you prefer to install Tailwind globally or already have it on your PATH,
you can skip this step — `build.rs` checks both locations.

## Run the Application

Everything is in place. Start the server:

```bash
cargo run
```

The first build will take a minute or two as Cargo downloads and compiles
dependencies. Subsequent builds are fast. You will see output like:

```
  INFO Database not configured
  INFO Listening addr=127.0.0.1:3000
```

The "Database not configured" message is expected — you have not set up
Postgres yet.

Open your browser and visit <http://localhost:3000>. You should see:

```
Welcome to todo-app!
```

Try the other routes:

- <http://localhost:3000/hello> — "Hello, Autumn!"
- <http://localhost:3000/hello/world> — "Hello, world!"
- <http://localhost:3000/health> — a JSON health check response (auto-mounted
  by the framework)
- <http://localhost:3000/actuator/health> — the actuator health view
- <http://localhost:3000/actuator/info> — build and runtime information

Press `Ctrl+C` in your terminal to stop the server. You will see:

```
  INFO Received Ctrl+C, starting graceful shutdown
  INFO Server shut down cleanly
```

Autumn handles graceful shutdown automatically — in-flight requests drain
before the process exits.

## What Just Happened

Here is what Autumn did when you called `.run().await`:

1. **Loaded configuration** from `autumn.toml` (falling back to defaults for
   anything missing)
2. **Initialized structured logging** based on the `[log]` section
3. **Skipped database pool creation** (no `[database]` section configured)
4. **Built an Axum router** from the routes you registered with `.routes()`
5. **Mounted framework routes** — the health check endpoint, actuator
   endpoints, and the bundled htmx JavaScript
6. **Served static files** from the `static/` directory
7. **Bound to `127.0.0.1:3000`** and started accepting connections

All of this from three lines in `main()`. The framework handles the
plumbing; you write handlers.

## Checkpoint

Your project should look like this:

```
todo-app/
+-- Cargo.toml
+-- autumn.toml
+-- build.rs
+-- src/
|   +-- main.rs
+-- static/
|   +-- css/
|       +-- input.css
|       +-- autumn.css   (generated by build.rs)
+-- tailwind.config.js
+-- migrations/
|   +-- .gitkeep
+-- target/
|   +-- autumn/
|       +-- tailwindcss  (downloaded by `autumn setup`)
+-- .gitignore
```

`cargo run` starts a server that responds at `http://localhost:3000` with
three routes (`/`, `/hello`, `/hello/{name}`) plus framework-provided
`/health` and `/actuator/*` endpoints.

This is your foundation. In the next chapter, you will replace the placeholder
routes with the beginnings of a todo application.

---

Next: [Chapter 2 — Routes and Handlers](02-routes.md)
