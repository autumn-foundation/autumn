# Chapter 11: Writing Integration Tests

**Goal:** By the end of this chapter you will have a `tests/` directory in your
todo-app with two passing tests — one smoke test that runs instantly and one
database round-trip test — using Autumn's first-party `TestApp`/`TestDb`
surface.

---

## Sections

### Why test with Autumn's utilities?

Autumn's `autumn_web::test` module gives you a fully-wired app in a single
method call. No mock framework, no hand-rolled `tower::ServiceExt::oneshot`,
no axum internals to understand. You get:

- The **same middleware stack** as production (security, tracing, rate-limiting,
  `RequestIdLayer`, …)
- **Real database** via a shared Postgres testcontainer — not a mock
- **Chainable assertions** on status, headers, and body
- **Zero new dependencies** beyond what `autumn new` already generated

The table below maps Autumn concepts to familiar analogies:

| Autumn | Spring Boot | Django |
|--------|-------------|--------|
| `TestApp` | `@SpringBootTest` | `TestCase` |
| `TestClient` | `MockMvc` | `Client` |
| `TestResponse` | `MvcResult` | `Response` |
| `TestDb` | `@DataJpaTest` + testcontainers | `TestCase` (transactional) |

---

### Step 1 — Add the dev-dependency

Open `Cargo.toml` and add a `[dev-dependencies]` section:

```toml
[dev-dependencies]
# autumn-web with test-support enables TestDb (shared Postgres testcontainer).
# The `db` feature is already active from [dependencies]; test-support adds TestDb.
autumn-web = { version = "0.7", features = ["test-support"] }
serde_json = "1"
```

`diesel`, `diesel_async`, `serde`, `tokio`, and `maud` are already in
`[dependencies]`, so they are available in test code without repeating them.

---

### Step 2 — Create `tests/integration_test.rs`

Create the file `tests/integration_test.rs` in the root of your project (next
to `Cargo.toml`, not inside `src/`).

Integration test files in `tests/` are compiled as their own crate and linked
against your package's public API. Because the todo-app is a binary (no
`src/lib.rs`), the test file cannot import your handler functions directly.
Instead, define the schema and handlers inline — the same approach Autumn uses
for its own `test_db_integration.rs`.

```rust
//! Integration tests for the todo-app.
//!
//! Run with:
//!
//!     cargo test                        # smoke tests (instant)
//!     cargo test -- --include-ignored   # + DB tests (needs Docker)

use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestDb};
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};

// ── Schema (matches your migration) ────────────────────────────────────────

diesel::table! {
    todos (id) {
        id -> Int8,
        title -> Text,
        completed -> Bool,
    }
}

// ── Model types ─────────────────────────────────────────────────────────────

#[derive(Debug, Queryable, Selectable, Serialize)]
#[diesel(table_name = todos)]
struct Todo {
    id: i64,
    title: String,
    completed: bool,
}

#[derive(Debug, Insertable, Deserialize)]
#[diesel(table_name = todos)]
struct NewTodo {
    title: String,
    #[serde(default)]
    completed: bool,
}

// ── Handlers under test ─────────────────────────────────────────────────────

#[get("/api/todos")]
async fn list_todos(mut db: Db) -> AutumnResult<Json<Vec<Todo>>> {
    let all = todos::table
        .select(Todo::as_select())
        .load(&mut *db)
        .await?;
    Ok(Json(all))
}

#[post("/api/todos")]
async fn create_todo(mut db: Db, Json(body): Json<NewTodo>) -> AutumnResult<Json<Todo>> {
    let created = diesel::insert_into(todos::table)
        .values(&body)
        .returning(Todo::as_returning())
        .get_result(&mut *db)
        .await?;
    Ok(Json(created))
}
```

---

### Step 3 — Write the smoke test

The smoke test boots the full Autumn middleware pipeline in-process and fires a
`GET` request — no Docker, no database required:

```rust
/// Smoke test — runs instantly, no Docker required.
///
/// Proves TestApp boots the full middleware stack. X-Request-Id is
/// Autumn-specific: RequestIdLayer adds it to every response.
#[tokio::test]
async fn smoke_test_get_returns_200_with_autumn_request_id() {
    #[get("/ping")]
    async fn ping() -> &'static str { "pong" }

    let client = TestApp::new().routes(routes![ping]).build();
    let resp = client.get("/ping").send().await;

    resp.assert_ok().assert_body_eq("pong");
    assert!(
        resp.header("x-request-id").is_some(),
        "Autumn's RequestIdLayer must attach X-Request-Id to every response"
    );
}
```

Run it now — no Docker needed:

```bash
cargo test
```

You should see one test pass instantly.

---

### Step 4 — Write the DB round-trip test

The database test uses `TestDb::shared()` to spin up a Postgres container.
Add a setup helper and the test:

```rust
// Setup: create the table and truncate for test isolation.
async fn setup_todos_table()
    -> diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>
{
    let db = TestDb::shared().await;
    db.execute_sql(
        "CREATE TABLE IF NOT EXISTS todos (
            id        BIGSERIAL PRIMARY KEY,
            title     TEXT    NOT NULL,
            completed BOOLEAN NOT NULL DEFAULT false
        )",
    ).await;
    db.execute_sql("TRUNCATE todos RESTART IDENTITY").await;
    db.pool()
}

/// DB round-trip — requires Docker (testcontainers starts Postgres).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn create_todo_round_trip() {
    let pool = setup_todos_table().await;
    let client = TestApp::new()
        .routes(routes![list_todos, create_todo])
        .with_db(pool)
        .build();

    // Initially empty
    client.get("/api/todos").send().await
        .assert_ok().assert_body_eq("[]");

    // Create a todo (DB write)
    let resp = client
        .post("/api/todos")
        .json(&serde_json::json!({"title": "Learn Autumn testing"}))
        .send().await;

    resp.assert_ok()
        .assert_header_contains("content-type", "application/json")
        .assert_json::<serde_json::Value, _>(|todo| {
            assert_eq!(todo["title"], "Learn Autumn testing");
            assert_eq!(todo["completed"], false);
            assert!(todo["id"].as_i64().unwrap() > 0);
        });

    // Confirm the write persisted (DB read)
    client.get("/api/todos").send().await
        .assert_ok()
        .assert_json::<Vec<serde_json::Value>, _>(|todos| {
            assert_eq!(todos.len(), 1);
            assert_eq!(todos[0]["title"], "Learn Autumn testing");
        });
}
```

The test is marked `#[ignore]` so `cargo test` stays green everywhere. Run
the DB test when Docker is available:

```bash
cargo test -- --include-ignored
```

---

### Checkpoint

Your project now has:

```
todo-app/
├── Cargo.toml          ← [dev-dependencies] section added
├── tests/
│   └── integration_test.rs   ← new
└── src/
    └── ...
```

Running `cargo test` produces:

```
test smoke_test_get_returns_200_with_autumn_request_id ... ok
test create_todo_round_trip ... ignored

test result: ok. 1 passed; 0 failed; 1 ignored
```

---

### What's next

- **Flash messages** — `autumn_web::test::TestResponse` can read flash cookies
  across redirects. See `docs/guide/testing.md` for the full pattern.
- **Authorization tests** — `TestApp::policy(…).scope(…)` lets you register
  policies in test scope.
- **Headless assertions** — pipe `resp.text()` through a regex or string search
  to verify rendered HTML from Maud templates.
- Compare your test file against the reference at
  `examples/todo-app/tests/integration_test.rs`.

---

Previous: [Chapter 10 — Configuration](10-configuration.md) | Next: [Chapter 12 — What's Next](12-whats-next.md)
