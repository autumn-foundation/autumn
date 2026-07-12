Prompt-Version: v1

# Build brief: Habit Tracker — Autumn

Read `prompts/_core.md` and `SPEC.md` first; they define the app, the exact JSON
API contract, and the streak semantics. Everything there applies. Below are the
Autumn-specific facts you need to build it.

## Framework facts

Autumn is a batteries-included Rust web framework (`autumn-web`) built on axum,
Diesel/`diesel-async` (Postgres), and Maud for HTML.

- **Dependency.** In-repo path dependency — in your app `Cargo.toml`:
  ```toml
  [package]
  edition = "2024"

  [dependencies]
  autumn-web = { path = "/home/user/autumn/autumn", features = ["seed"] }
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
  autumn-web = { path = "/home/user/autumn/autumn", features = ["test-support"] }
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

## Scaffolding options

- Fastest: **copy `/home/user/autumn/examples/todo-app`** as a starting point and
  adapt it — it already wires migrations, a Diesel model, Maud HTML routes, JSON
  API routes, `Json<T>`, validation, a `seed` bin, and `TestApp`/`TestDb` tests.
  `/home/user/autumn/examples/bookmarks` is a second, simpler reference.
- Or scaffold a fresh app with the CLI:
  `cargo run -p autumn-cli -- new habit-tracker` (run from `/home/user/autumn`).

Study those two examples — `todo-app` and `bookmarks` — for the canonical
Autumn patterns before writing code. Do not invent APIs; mirror what the
examples do.

## run.sh

Provide `run.sh` that sets `DATABASE_URL` (Postgres) if unset, runs `cargo run`
(migrations run automatically via `.migrations(...)`, seed via the `seed` bin or
an `on_startup` hook), and binds the server to `PORT` (default 8080). It must
block while the server runs.
