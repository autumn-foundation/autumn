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
   ✗ no seed binary found; create `src/bin/seed.rs`
   See: https://autumn.rs/guide/seeding
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

## Faking realistic volume data

Hand-writing a few fixture rows is fine for a handful of records, but list
views — pagination, full-text search, sort/filter, CSV export — need
hundreds of *varied* rows before you can tell whether they actually work.
Every `#[autumn_web::model]` gets a generated `{Model}Factory` whose
`.fake()` fills any field you didn't explicitly set with realistic data
inferred from the field's name and type (an `email` field gets a fake email,
a `title` gets fake words, a `created_at` gets a recent timestamp, and so
on).

### One-line faked seed

```rust
use autumn_web::seed::SeedContext;

#[autumn_web::main]
async fn main() {
    let ctx = SeedContext::build().expect("seed context");

    // 200 faked posts, each with distinct fake title/body — enough to
    // exercise pagination and search.
    Post::factory().fake().create_many(200, ctx.pool()).await;
}
```

`.fake()` never overwrites a field you set explicitly:

```rust
// `title` stays fixed; every other field is faked.
Post::factory().title("Pinned announcement").fake().create(&pool).await;
```

Other factory methods that pair with `.fake()`:

| Method | Description |
|--------|-------------|
| `.fake()` / `.fake_all()` | Fill every unset field with a fake value (aliases of each other). |
| `.build()` | Construct one in-memory instance without persisting it. |
| `.build_many(n)` | Construct `n` in-memory instances, each faked independently. |
| `.create(&pool)` | Persist one instance. |
| `.create_many(n, &pool)` | Persist `n` instances, returning `Vec<Model>`. |

See `examples/bookmarks/src/bin/seed.rs` and its
[README](../../examples/bookmarks/README.md#seeding-fake-data) for a
complete working example that populates 200 rows this way.

### The `fake` module directly

`autumn_web::fake` also works standalone, outside a factory, when you want a
realistic value for one field: `fake::name()`, `fake::username()`,
`fake::email()`, `fake::word()` / `fake::words(n)` / `fake::sentence()` /
`fake::paragraph()`, `fake::url()`, `fake::boolean()`,
`fake::int_range(lo, hi)`, `fake::decimal()`, `fake::recent_datetime()`, and
`fake::uuid()`.

### Reproducible fake data

Set `AUTUMN_FAKE_SEED` to a `u64` to make every `fake::*` call and every
`.fake()`-driven factory deterministic — the same sequence of calls always
produces the same values, which keeps golden-data tests and CI fixtures
reproducible:

```sh
AUTUMN_FAKE_SEED=42 autumn seed --count 200 --model Post
```

Without `AUTUMN_FAKE_SEED` set, output varies from run to run. Tests can
call `autumn_web::fake::reseed(seed)` directly instead of setting the env
var.

### Generating rows without editing `src/bin/seed.rs`

`autumn seed --count N --model M` drives a registered model's factory
directly — generate and insert `N` faked rows for model `M` without touching
your seed binary at all:

```sh
autumn seed --count 200 --model Post
```

`--count` and `--model` must be passed together; passing neither preserves
the default behavior described above (run `src/bin/seed.rs`). Every
`#[autumn_web::model]` registers itself automatically, so any scaffolded
model is reachable this way as soon as it exists — projects generated with
`autumn new --with-seed` (or a model added via `autumn generate scaffold`)
already wire their `src/bin/seed.rs` to handle this request via
`autumn_web::seed::maybe_fake_seed`, so `--count`/`--model` work out of the
box with no manual edits.

---

## Out of scope

- **Test fixtures** — use `autumn_web::test` helpers for integration test data.
- **YAML/JSON/CSV loaders** — author a thin loader inside your seed binary if
  you want declarative fixtures.
- **Relationship-aware faking** — `.fake()` fills scalar fields on a single
  model; it does not know which rows exist in a parent table, so a foreign-key
  field left unset gets a plain fake integer (or stays at `Default::default()`
  without `.fake()`) rather than a valid parent id — either way, inserting
  without addressing the FK will fail. For a model with a foreign key, either
  set that field explicitly (`.user_id(id)`) or mark it `#[factory_assoc(Type)]`
  in the `#[model]` definition so the factory creates (or accepts) a parent
  automatically; see the `#[factory_assoc]` docs for the associated-model
  factory pattern.
- **`autumn generate seed`** — tracked in #493 follow-up work.

---

## See also

- [`docs/guide/getting-started.md`](getting-started.md) — includes
  `autumn seed` in the quickstart flow
- [`docs/guide/generators.md`](generators.md) — `autumn generate model` / scaffold
- [`docs/guide/console.md`](console.md) — `autumn console`, the data playground
  that shares this `SeedContext` bootstrap
