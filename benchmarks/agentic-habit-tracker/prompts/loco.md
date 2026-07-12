Prompt-Version: v1

# Build brief: Habit Tracker — Loco (Rust)

Read `prompts/_core.md` and `SPEC.md` first; they define the app, the exact JSON
API contract, and the streak semantics. Everything there applies. Below are the
Loco-specific bootstrapping notes.

## Framework facts

- Use **Loco** (`loco-rs`), the Rails-inspired Rust framework built on axum +
  SeaORM.
- Scaffold: `cargo install loco` (or `loco-cli`), then
  `loco new` and pick the **SaaS / server-side-rendered** starter (you need an
  HTML view), backed by Postgres or SQLite.
- **Models / migrations.**
  - Generate a `habits` model (`name` string, `description` nullable text,
    `created_at` timestamp) and a `completions` model (`habit_id` FK, `day` date)
    via `cargo loco generate model ...` / migration files under `migration/`.
  - Add a unique index on `(habit_id, day)` in the migration; a SeaORM insert
    that violates it returns a DB error you map to **409**. Add an
    `ON DELETE CASCADE` FK so completions are removed with their habit.
  - Run migrations with `cargo loco db migrate`.
- **Controllers / routes.** Loco controllers are axum handlers grouped into
  `Routes` and registered in `app.rs`:
  - JSON endpoints under `/api/habits` (create/list/get/update/delete/complete),
    returning `Result<Response>` with explicit statuses via
    `format::json(...)` and `axum::http::StatusCode` (`201`, `204`, `409`, `422`,
    `404`).
  - An HTML endpoint `GET /` returning a server-rendered view (Tera/`format::html`)
    listing habits with `text/html`.
- **Validation.** Use the `validator` crate on request structs (`#[validate(...)]`)
  → 422. Parse the completion date with
  `chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")`; parse error → 422/400.
- **Streak.** Compute `current_streak` per `SPEC.md` §3 in Rust from the
  completion dates; `history` = dates sorted descending as ISO `YYYY-MM-DD`
  strings.
- **Seed / demo data.** Loco supports seeds (`src/fixtures` / a `seed` task or a
  `cargo loco task`); insert a couple of demo habits with completions.
- **Tests.** `cargo test` with Loco's `testing` helpers (`request` /
  `boot_test`) hitting the endpoints.

## run.sh

```sh
#!/usr/bin/env sh
set -e
cargo loco db migrate
cargo loco task seed || true                # your seed task
exec cargo loco start --server-and-worker --binding 0.0.0.0 --port "${PORT:-8080}"
```

`cargo loco start` blocks while the server runs. (Adjust flags to your Loco
version; the key requirement is binding to `PORT`, default 8080.)
