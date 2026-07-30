# Database Seeding

Autumn ships a first-class seed convention so a freshly-migrated database can
be populated with representative data in a single command.

```sh
autumn migrate && autumn seed
```

---

## The convention

Seed code lives in `src/bin/seed.rs` — an ordinary Cargo binary that receives
a database connection through [`autumn_web::seed::SeedContext`].  No special
DSL, no template language, and no duplicated connection wiring: seed code uses
the same `#[model]` / `#[repository]` types the application uses, so the
compiler keeps everything in sync.

The binary is discovered by `autumn seed` through the Cargo binary target
named `seed`.

---

## Quick start

### 1. Add the `seed` feature to `autumn-web`

```toml
# Cargo.toml
[dependencies]
autumn-web = { version = "0.5", features = ["seed"] }

[[bin]]
name = "seed"
path = "src/bin/seed.rs"
```

Or scaffold it automatically when creating a new project:

```sh
autumn new my-app --with-seed
```

### 2. Write `src/bin/seed.rs`

```rust
use autumn_web::seed::SeedContext;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use my_app::schema::posts;

#[derive(Insertable)]
#[diesel(table_name = posts)]
struct NewPost<'a> {
    title: &'a str,
    body: &'a str,
}

#[tokio::main]
async fn main() {
    let ctx = SeedContext::build().expect("seed context");
    println!("Seeding ({})...", ctx.profile());

    let mut db = ctx.conn().await.expect("db connection");

    // Idempotency guard — skip if the table already has data.
    let count: i64 = posts::table.count().get_result(&mut *db).await.unwrap_or(0);
    if count > 0 {
        println!("Already seeded; skipping.");
        return;
    }

    diesel::insert_into(posts::table)
        .values(&[
            NewPost { title: "Hello, world!", body: "My first post." },
            NewPost { title: "Getting started", body: "Autumn makes it easy." },
        ])
        .execute(&mut *db)
        .await
        .expect("insert failed");

    println!("Seeded 2 posts.");
}
```

### 3. Run

```sh
# Run migrations first (autumn seed will error if pending migrations exist)
autumn migrate

# Seed the database
autumn seed

# Use a non-default profile
autumn seed --profile demo
```

---

## How it works

`autumn seed` does four things:

1. **Checks that `src/bin/seed.rs` exists.** If it does not, you get a clear
   error:
   ```
   ✗ no seed binary found; create `src/bin/seed.rs` or run `autumn generate seed`
   ```

2. **Checks for pending migrations** (when the `diesel` CLI is available).
   Seeds run *after* migrations; if any are pending, you see:
   ```
   ✗ pending migrations detected; run `autumn migrate` before `autumn seed`
   ```

3. **Sets the profile** via the `AUTUMN_ENV` environment variable (default:
   `dev`). Your seed binary reads `ctx.profile()` to branch on environment.

4. **Delegates to `cargo run --bin seed`**. All Cargo flags such as `--package`
   work:
   ```sh
   autumn seed --profile demo --package my-workspace-member
   ```

---

## Profile-aware seeding

`SeedContext::build()` reads the profile from `AUTUMN_ENV` (or `AUTUMN_PROFILE`
as a legacy alias), which `autumn seed` sets automatically. Use `ctx.profile()`
to vary the seed data between environments:

```rust
let items: Vec<_> = match ctx.profile() {
    "demo" => demo_items(),
    _ => dev_items(),
};
```

---

## Idempotency pattern

Autumn does not enforce idempotency — that is your responsibility. Two common
patterns:

### Count-based guard (simplest)

```rust
let count: i64 = my_table::table.count().get_result(&mut *db).await.unwrap_or(0);
if count > 0 {
    println!("Already seeded; skipping.");
    return;
}
```

Re-running inserts nothing if the table already has rows.

### Upsert-by-natural-key

If your table has a unique index on a natural key (e.g. `slug`), use
`ON CONFLICT DO NOTHING`:

```rust
diesel::insert_into(posts::table)
    .values(&seed_data)
    .on_conflict(posts::slug)
    .do_nothing()
    .execute(&mut *db)
    .await?;
```

Re-running skips rows whose slug already exists.

---

## `SeedContext` API reference

```rust
/// Build a seed context from environment + autumn.toml.
pub fn build() -> Result<SeedContext, SeedContextError>

/// Active profile (e.g. "dev", "demo", "test").
pub fn profile(&self) -> &str

/// Acquire a pooled connection.
pub async fn conn(&self) -> Result<Object<AsyncPgConnection>, SeedContextError>
```

`Object<AsyncPgConnection>` implements `DerefMut` to `AsyncPgConnection`, so
pass it to diesel-async query methods as `&mut *conn`.

---

## Example: `examples/todo-app`

The canonical `todo-app` example ships a complete seed at
`examples/todo-app/src/bin/seed.rs`.  Its idempotency guard uses the
count-based pattern: if any todos already exist, the seed exits early.

```sh
cd examples/todo-app
autumn migrate && autumn seed && autumn dev
# → localhost:3000 shows five pre-populated todos
```

---

## Out of scope

- **Test fixtures** — use `autumn_web::test` helpers for integration test data.
- **YAML/JSON/CSV loaders** — author a thin loader inside your seed binary if
  you want declarative fixtures.
- **Faker / factory libraries** — compose with `fake`, `proptest`, `rand`, or
  whatever you prefer; Autumn owns the runner, not the data generation.
- **`autumn generate seed`** — tracked in #493 follow-up work.

---

## See also

- [`docs/guide/getting-started.md`](getting-started.md) — includes
  `autumn seed` in the quickstart flow
- [`docs/guide/generators.md`](generators.md) — `autumn generate model` / scaffold
- [`docs/guide/console.md`](console.md) — `autumn console`, the data playground
  that shares this `SeedContext` bootstrap
