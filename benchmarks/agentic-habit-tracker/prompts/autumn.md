Prompt-Version: v1

# Build brief: Habit Tracker — Autumn

Read `prompts/_core.md` and `SPEC.md` first; they define the app, the exact JSON
API contract, and the streak semantics. Everything there applies. Below are the
Autumn-specific facts you need to build it.

## Framework facts

Autumn is a batteries-included Rust web framework (`autumn-web`) built on axum,
Diesel/`diesel-async` (Postgres), and Maud for HTML.

- **Dependency.** Depend on the in-repo Autumn crate. This benchmark lives at
  `<repo-root>/benchmarks/agentic-habit-tracker/` and the crate is at
  `<repo-root>/autumn`. Discover the repo root with
  `git rev-parse --show-toplevel` — do **not** hardcode an absolute home path
  such as one under `/home/.../autumn`. From
  your app at `runs/<id>/app/`, that crate is five levels up
  (`app` → `<id>` → `runs` → `agentic-habit-tracker` → `benchmarks` →
  repo root, then into `autumn/`), so use the relative path
  `path = "../../../../../autumn"`. You may instead use an **absolute** path to
  this checkout's `autumn/` directory, or depend on the published crate
  (`autumn-web = "0.6"`). In your app `Cargo.toml`:
  ```toml
  [package]
  edition = "2024"

  [dependencies]
  # Relative path from runs/<id>/app/ to <repo-root>/autumn (five levels up).
  # Or use an absolute path from `git rev-parse --show-toplevel`, or `autumn-web = "0.6"`.
  autumn-web = { path = "../../../../../autumn", features = ["seed"] }
  diesel = { version = "2", features = ["postgres", "chrono"] }
  diesel-async = { version = "0.9", features = ["postgres"] }
  diesel_migrations = "2"
  maud = { version = "0.27", features = ["axum"] }
  serde = { version = "1", features = ["derive"] }
  serde_json = "1"
  chrono = { version = "0.4", features = ["serde"] }
  tokio = { version = "1", features = ["full"] }
  validator = { version = "0.20", features = ["derive"] }

  [dev-dependencies]
  autumn-web = { path = "../../../../../autumn", features = ["test-support"] }
  ```

- **Routes.** Handlers are annotated with `#[get("/path")]` / `#[post("/path")]`
  / `#[put(...)]` / `#[delete(...)]` and collected with the `routes![...]` macro,
  which is passed to the app builder:
  ```rust
  autumn_web::app()
      .migrations(MIGRATIONS)
      .routes(routes![
          routes::web::index,
          routes::api::create_habit,
          routes::api::list_habits,
          routes::api::get_habit,
          routes::api::update_habit,
          routes::api::delete_habit,
          routes::api::complete_habit,
      ])
      .run()
      .await;
  ```
  Use `#[autumn_web::main]` on `async fn main()`.

- **JSON.** Extract request bodies with `Json<T>` (where `T: Deserialize`) and
  return `AutumnResult<Json<T>>` / `impl IntoResponse`. Import
  `use autumn_web::prelude::*;` — it re-exports `Json`, `State`, `AppState`,
  `AutumnResult`, `StatusCode`, and the route attribute macros. To control the
  status code, return a tuple like `(StatusCode::CREATED, Json(body))` or an
  `impl IntoResponse`.

- **Persistence (Diesel).** Define the schema with the `table!` macro in
  `schema.rs`:
  ```rust
  diesel::table! {
      habits (id) { id -> Int8, name -> Text, description -> Nullable<Text>, created_at -> Timestamp }
  }
  diesel::table! {
      completions (id) { id -> Int8, habit_id -> Int8, day -> Date }
  }
  ```
  Put a UNIQUE `(habit_id, day)` constraint in the migration so duplicate
  completions can be detected → map the DB unique-violation to **409**. Models
  are `#[derive(Queryable, Selectable, Serialize)]` structs; inserts use
  `#[derive(Insertable, Deserialize, Validate)]` "New" structs. Get a DB handle
  in a handler via the `Db` extractor, which yields an `AsyncPgConnection`:
  ```rust
  async fn list_habits(mut db: Db) -> AutumnResult<Json<Vec<Habit>>> { ... }
  ```
  Run queries with `diesel_async::RunQueryDsl` (`.load(&mut db).await`,
  `.get_result(&mut db).await`, etc.).

- **Migrations.** Embed them with
  `const MIGRATIONS: EmbeddedMigrations = embed_migrations!();`
  (`use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};`) and register
  with `.migrations(MIGRATIONS)`. Migration SQL lives under `migrations/`.
  Higher-level `#[autumn_web::model]` / `#[repository]` macros also exist if you
  prefer them, but plain Diesel as above is the most direct path.

- **Server-side HTML (Maud).** Return `maud::Markup` from a handler; build markup
  with the `html! { ... }` macro. Set the response content type to `text/html`
  (Maud's axum integration does this for `Markup`). Example:
  ```rust
  #[get("/")]
  async fn index(mut db: Db) -> AutumnResult<Markup> {
      let habits = Habit::all(&mut db).await?;
      Ok(html! { h1 { "Habits" } ul { @for h in &habits { li { (h.name) } } } })
  }
  ```

- **Validation.** Use the `validator` crate derives (`#[validate(length(min = 1))]`
  on `name`) and return **422** on failure. Parse dates with
  `chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")`; a parse error → 422/400.

- **Tests.** `autumn_web::prelude` exposes `TestApp` and `TestDb` (needs the
  `test-support` feature). `TestDb` spins up a throwaway Postgres; `TestApp`
  drives your router in-process so you can assert on JSON responses. Write
  integration tests under `tests/`.

## Scaffolding (recommended first)

**Fastest path — scaffold with the CLI, then fill in the streak logic and the
JSON contract.** Hand-building by copying `examples/` is the fallback. All CLI
commands can be run in-repo via `cargo run -p autumn-cli -- <args>` (run from the
repo root — find it with `git rev-parse --show-toplevel`); if `autumn` is on your
`PATH`, `autumn <args>` is equivalent.

- **Create the app.** `autumn new habit-tracker` scaffolds a fresh project
  (minimal base by default). Pass `--starter <name>` to start from a curated
  starter, and `--list-starters` to see what's available. In-repo:
  `cargo run -p autumn-cli -- new habit-tracker`.
- **Scaffold a resource in one step.**
  `autumn generate scaffold Habit name:String description:Text` generates the
  `#[model]` struct, a Diesel migration, a `#[repository]`, HTML routes, and a
  smoke test, and registers the new routes in `src/main.rs`. Field DSL supports
  `name:Type`, `field:references` (FK), `field:Type:unique`, `enum{a,b,c}`, and
  `Option<...>`; `--api` emits a JSON-only resource. Narrower generators exist
  too: `autumn generate model|migration|task|job|mailer ...`. In-repo:
  `cargo run -p autumn-cli -- generate scaffold Habit name:String description:Text`.
- **Iterate and verify.** `autumn dev` runs the dev server with hot reload
  (watch mode); `autumn migrate` runs pending migrations; `autumn test`
  provisions the test database, migrates it, and runs `cargo test`.

After scaffolding, adapt the generated code to match the exact JSON contract in
`SPEC.md` (status codes, the `complete`/streak endpoints, and the
`current_streak`/`history` computation are yours to implement — the scaffold
won't produce them verbatim).

## Reference examples (fallback: hand-build)

- **Copy `examples/todo-app`** (at the repo root of this checkout —
  `<repo-root>/examples/todo-app`; the repo root is discoverable via
  `git rev-parse --show-toplevel`) as a starting point and adapt it — it already
  wires migrations, a Diesel model, Maud HTML routes, JSON API routes, `Json<T>`,
  validation, a `seed` bin, and `TestApp`/`TestDb` tests. `<repo-root>/examples/bookmarks`
  is a second, simpler reference.

Study those two examples — `todo-app` and `bookmarks` — for the canonical
Autumn patterns before writing code. Do not invent APIs; mirror what the
examples (and the CLI-generated scaffold) do.

## run.sh

Provide `run.sh` that sets `DATABASE_URL` (Postgres) if unset, runs `cargo run`
(migrations run automatically via `.migrations(...)`, seed via the `seed` bin or
an `on_startup` hook), and binds the server to `PORT` (default 8080). It must
block while the server runs.
