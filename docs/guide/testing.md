# Integration Testing

Autumn ships a first-party test surface (`autumn_web::test`) that brings
Rails-grade ergonomics to Rust integration testing: one line to boot a fully-wired
app, assertions that chain, and a shared Postgres testcontainer that keeps your
test suite fast.

> **Reference implementation:** `examples/blog/tests/integration_test.rs`
> exercises every concept on this page against real blog routes.

---

## The public surface

| Type | Purpose | Spring Boot analogy |
|------|---------|---------------------|
| [`TestApp`] | Boot a fully-wired Autumn app in-process | `@SpringBootTest` |
| [`TestClient`] | Fluent HTTP request builder | `MockMvc` / `WebTestClient` |
| [`TestResponse`] | Response with chainable assertion helpers | `MvcResult` |
| [`TestDb`] | Shared Postgres testcontainer | `@DataJpaTest` |

[`TestApp`]: https://docs.rs/autumn-web/latest/autumn_web/test/struct.TestApp.html
[`TestClient`]: https://docs.rs/autumn-web/latest/autumn_web/test/struct.TestClient.html
[`TestResponse`]: https://docs.rs/autumn-web/latest/autumn_web/test/struct.TestResponse.html
[`TestDb`]: https://docs.rs/autumn-web/latest/autumn_web/test/struct.TestDb.html

`TestApp` fires requests through the full Axum middleware pipeline using
`tower::ServiceExt::oneshot()` — the same security, tracing, rate-limiting,
and routing stack you run in production, minus the TCP listener.

---

## Quick start — no Docker required

Add `autumn_web::test` to your integration test and write your first assertion:

```rust
// tests/integration_test.rs
use autumn_web::prelude::*;
use autumn_web::test::TestApp;

#[get("/hello")]
async fn hello() -> &'static str { "Hello, Autumn!" }

#[tokio::test]
async fn hello_returns_200() {
    let client = TestApp::new()
        .routes(routes![hello])
        .build();

    client.get("/hello").send().await
        .assert_ok()
        .assert_body_contains("Autumn");
}
```

Run it:

```bash
cargo test
```

No Docker, no extra setup. `TestApp::new()` uses the `"test"` profile by
default and disables CSRF so form submissions work without a session token.

---

## Autumn-specific assertions

Every Autumn response carries `X-Request-Id` (set by `RequestIdLayer`). You
can assert on it as a framework-level signal that the full middleware stack ran:

```rust
#[tokio::test]
async fn autumn_attaches_request_id_to_every_response() {
    let client = TestApp::new().routes(routes![hello]).build();
    let resp = client.get("/hello").send().await;

    assert!(
        resp.header("x-request-id").is_some(),
        "Autumn's RequestIdLayer must attach X-Request-Id to every response"
    );
}
```

Other useful assertions on `TestResponse`:

```rust
resp
    .assert_ok()                                   // 200 OK
    .assert_status(201)                            // specific status
    .assert_success()                              // any 2xx
    .assert_header("content-type", "text/plain")   // exact header value
    .assert_header_contains("content-type", "json") // substring
    .assert_body_contains("Alice")                 // body substring
    .assert_body_eq("pong")                        // exact body
    .assert_body_empty()                           // empty body
    .assert_json::<MyType, _>(|val| {              // deserialize + check
        assert_eq!(val.name, "Alice");
    });
```

### Asserting channel broadcasts

When a handler publishes to a channel (`ws` feature), opt in with
`TestApp::record_broadcasts()` to capture every publication a request makes —
no hand-written spy or `Arc<Mutex>`. The recorder installs through the existing
`ChannelsInterceptor` seam, is scoped to the `TestClient` (parallel tests never
leak into one another), and is zero-cost when you don't call it — production
`Channels` behavior is untouched. Both raw `publish` text and `publish_html`
HTML/OOB payloads are recorded.

```rust
#[post("/notes")]
async fn create_note(State(state): State<AppState>) -> &'static str {
    state.broadcast().publish("notes", "created").unwrap();
    "ok"
}

#[tokio::test]
async fn publishing_broadcasts_on_create() {
    let client = TestApp::new()
        .routes(routes![create_note])
        .record_broadcasts()          // opt in
        .build();

    client.post("/notes").send().await.assert_ok();

    client
        .assert_broadcast_count("notes", 1)                       // exactly one publish
        .assert_broadcast("notes", |b| b.payload() == "created")  // a matching payload
        .assert_no_broadcasts("audit");                           // nothing elsewhere
}
```

| Method | Checks |
|--------|--------|
| `record_broadcasts()` | builder — opt in to recording (on `TestApp`) |
| `broadcasts()` | every recorded publication, in publish order |
| `broadcasts_on(topic)` | recorded publications on `topic`, in order |
| `assert_broadcast(topic, predicate)` | at least one publish on `topic` matches |
| `assert_broadcast_count(topic, n)` | exactly `n` publishes on `topic` |
| `assert_no_broadcasts(topic)` | nothing was published to `topic` |

Each `RecordedBroadcast` exposes `.topic()` and `.payload()`. On failure the
`assert_broadcast*` helpers self-diagnose: they list what *was* published to the
topic and, grouped, to every other topic. Reading or asserting without
`record_broadcasts()` panics with a message pointing you at the builder.

---

## Structural HTML assertions

Autumn renders server-side HTML (Maud + htmx), so tests should assert on a
page's *structure* rather than brittle substrings. `TestResponse` parses the
body with a real HTML parser and matches against a CSS-selector subset, so
assertions survive cosmetic template changes — whitespace, attribute order, or
extra wrapping markup — that would break an `assert_body_contains` check. They
work for full documents and for partial/fragment responses (htmx swaps) alike.

Supported selectors: tag (`tr`), `.class`, `#id`, `[attr]` / `[attr=v]` /
`[attr^=v]` / `[attr$=v]` / `[attr*=v]`, compound selectors (`a.link[href]`),
selector lists (`a, button`), and descendant (`table tr`) / child
(`tbody > tr`) combinators.

```rust
#[tokio::test]
async fn notes_index_renders_one_row_per_note() {
    let client = TestApp::new().routes(routes![notes_index]).build();

    client.get("/notes").send().await
        .assert_ok()
        .assert_selector("table.notes")                  // the table is present
        .assert_selector_count("tbody tr.note-row", 3)   // exactly three rows
        .assert_attr("tr.note-row a", "href", "/notes/1") // first row's link target
        .assert_text("tr.note-row a", "First note")      // …and its visible text
        .assert_no_selector(".flash--error");            // no error flash rendered
}
```

| Method | Checks |
|--------|--------|
| `assert_selector(css)` | at least one element matches |
| `assert_no_selector(css)` | no element matches |
| `assert_selector_count(css, n)` | exactly `n` elements match |
| `assert_text(css, expected)` | first match's text equals `expected` (whitespace-normalized) |
| `assert_text_contains(css, sub)` | first match's text contains `sub` |
| `assert_attr(css, attr, expected)` | first match's `attr` equals `expected` |

For custom assertions, the non-asserting accessors return owned data:
`selector_count(css) -> usize`, `selector_text(css) -> Vec<String>`, and
`selector_attr(css, attr) -> Vec<Option<String>>` — each in document order.

```rust
let hrefs = resp.selector_attr("tbody tr.note-row a", "href");
assert_eq!(hrefs, vec![Some("/notes/1".into()), Some("/notes/2".into())]);
```

On failure these print the selector, the expected-vs-actual value, and a
truncated outline of the parsed HTML — so a red test points straight at the
mismatch instead of dumping raw markup.

---

## Database integration tests

For tests that need a real database, `TestDb` wraps a Postgres testcontainer.
The container starts once per test binary and is shared across all tests —
no one-container-per-test overhead.

### 1  Add the dev-dependency

```toml
# Cargo.toml  — use the same version as your [dependencies] entry
[dev-dependencies]
autumn-web = { version = "0.5", features = ["test-support"] }
serde_json = "1"
```

Replace `"0.4"` with whatever version you have in `[dependencies]` (or omit
`version` entirely and rely on Cargo's workspace resolution).

The `test-support` feature activates `TestDb`. No other dev-dependency is
needed; `diesel`, `diesel-async`, `serde`, and `tokio` are already in your
`[dependencies]`.

### 2  Define your schema and handlers inline

Integration tests in `tests/` are separate crates that cannot import from
`src/main.rs` (binary crates don't expose a library target). Define the schema
and handler under test inline — or extract them into a `src/lib.rs` for larger
apps:

```rust
// tests/integration_test.rs
use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestDb};
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};

diesel::table! {
    posts (id) {
        id -> Int8,
        title -> Text,
        slug -> Text,
        body -> Text,
        published -> Bool,
    }
}

#[derive(Queryable, Selectable, Serialize)]
#[diesel(table_name = posts)]
struct Post { id: i64, title: String, slug: String, body: String, published: bool }

#[derive(Insertable, Deserialize)]
#[diesel(table_name = posts)]
struct NewPost { title: String, slug: String, body: String, #[serde(default)] published: bool }

#[get("/api/posts")]
async fn list_published(mut db: Db) -> AutumnResult<Json<Vec<Post>>> {
    let rows = posts::table
        .filter(posts::published.eq(true))
        .select(Post::as_select())
        .load(&mut *db).await?;
    Ok(Json(rows))
}

#[post("/api/posts")]
async fn create_post(mut db: Db, Json(body): Json<NewPost>) -> AutumnResult<Json<Post>> {
    let created = diesel::insert_into(posts::table)
        .values(&body)
        .returning(Post::as_returning())
        .get_result(&mut *db).await?;
    Ok(Json(created))
}
```

### 3  Spin up the container and run your test

```rust
async fn setup() -> diesel_async::pooled_connection::deadpool::Pool<
    diesel_async::AsyncPgConnection
> {
    let db = TestDb::shared().await;           // shared container — starts once
    db.execute_sql(
        "CREATE TABLE IF NOT EXISTS posts (
            id BIGSERIAL PRIMARY KEY,
            title TEXT NOT NULL,
            slug  TEXT NOT NULL DEFAULT '',
            body  TEXT NOT NULL DEFAULT '',
            published BOOLEAN NOT NULL DEFAULT false
        )",
    ).await;
    db.execute_sql("TRUNCATE posts RESTART IDENTITY").await;
    db.pool()
}

/// DB round-trip: create a post, then verify it appears in the listing.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn create_post_round_trip() {
    let pool = setup().await;
    let client = TestApp::new()
        .routes(routes![list_published, create_post])
        .with_db(pool)
        .build();

    // Initially empty
    client.get("/api/posts").send().await.assert_ok().assert_body_eq("[]");

    // DB write
    client
        .post("/api/posts")
        .json(&serde_json::json!({
            "title": "Hello from Autumn tests",
            "slug":  "hello-autumn-tests",
            "body":  "Created in an integration test.",
            "published": true
        }))
        .send().await
        .assert_ok()
        .assert_header_contains("content-type", "application/json")
        .assert_json::<serde_json::Value, _>(|post| {
            assert_eq!(post["title"], "Hello from Autumn tests");
            assert!(post["id"].as_i64().unwrap() > 0);
        });

    // DB read — confirm the write persisted
    client.get("/api/posts").send().await
        .assert_ok()
        .assert_json::<Vec<serde_json::Value>, _>(|posts| {
            assert_eq!(posts.len(), 1);
            assert_eq!(posts[0]["title"], "Hello from Autumn tests");
        });
}
```

### 4  Run the tests

```bash
# smoke tests only (instant, no Docker)
cargo test

# include Docker-backed DB tests
cargo test -- --include-ignored

# or opt-in via an env var in CI
cargo test -- --include-ignored   # set DOCKER_HOST or TESTCONTAINERS_HOST
```

---

## Why `#[ignore = "requires Docker"]`?

Marking DB tests as `#[ignore]` means `cargo test` (no flags) runs green
everywhere — CI machines without Docker, laptops without a running daemon,
etc. Developers who have Docker available opt in with `--include-ignored`.

This mirrors how Autumn's own test suite handles `test_db_integration.rs`.

---

## Running doctests

The `autumn_web::test` module itself ships runnable doctests. Run them with:

```bash
cargo test --doc -p autumn-web
```

---

## Test-data factories

Every `#[model]` type automatically gets a `{Model}Factory` builder. Tests
declare only the fields that matter — everything else stays at its type default
(`""` for `String`, `0` for integers, `false` for `bool`, `None` for
`Option<T>`).

### Declaring a model

```rust
// src/models.rs
use crate::schema::posts;

#[autumn_web::model]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub slug: String,
    pub body: String,
    pub published: bool,
    pub views: i32,
}
```

The macro generates `Post::factory()` → `PostFactory`, along with setter
methods for each non-ID field and two terminus methods:

| Method | Returns | Description |
|--------|---------|-------------|
| `.build()` | `NewPost` | In-memory struct, no database |
| `.create(&pool).await` | `Post` | Insert + return with PK populated |

### Building in-memory instances

```rust
// Zero required args — only override what your test cares about
let draft: NewPost = Post::factory().build();
assert_eq!(draft.title, "");

// Override one field; all others stay at default
let draft = Post::factory().title("Hello TDD").build();
assert_eq!(draft.title, "Hello TDD");
assert_eq!(draft.views, 0);            // untouched

// Override several fields
let draft = Post::factory()
    .title("Published piece")
    .slug("published-piece")
    .published(true)
    .build();
assert!(draft.published);
assert_eq!(draft.body, "");            // untouched
```

### Persisting to the database

Use `.create(&pool)` when a test needs a real DB row:

```rust
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn post_appears_in_listing() {
    let db = TestDb::shared().await;
    // (run CREATE TABLE ... first)

    let post = Post::factory()
        .title("TDD post")
        .published(true)
        .create(&db.pool())
        .await;

    assert!(post.id > 0);
    assert_eq!(post.title, "TDD post");
}
```

### Factory composition

When one model references another (e.g. a `Comment` that belongs to a `Post`),
build the parent first and pass its primary key:

```rust
// 1. Persist the parent
let post = Post::factory().title("Parent").create(&db.pool()).await;

// 2. Build the child referencing the parent's id
let comment = Comment::factory()
    .post_id(post.id)
    .body("Great read!")
    .create(&db.pool())
    .await;

assert_eq!(comment.post_id, post.id);
```

This keeps associations explicit and avoids hidden global state or
infinite-recursion footguns.

### Line-count benchmark

The success metric from the original spec: a "create user with one published
post and one comment" fixture should be **≤ 8 lines** of intent, down from
**≥ 25 lines** of struct-literal boilerplate:

```rust
// Before: 25+ lines of NewUser { id: 1, email: "a@b.c".into(), ... }
// After factory pattern:
let user    = User::factory().email("alice@example.com").create(&pool).await;
let post    = Post::factory().user_id(user.id).title("Hello").published(true).create(&pool).await;
let comment = Comment::factory().post_id(post.id).body("Nice!").create(&pool).await;
```

---

## Patterns at a glance

| Scenario | Pattern |
|----------|---------|
| No-DB smoke test | `TestApp::new().routes(routes![...]).build()` |
| Custom config | `.config(AutumnConfig { … })` or `.profile("staging")` |
| With database | `.with_db(TestDb::shared().await.pool())` |
| Authorization | `.policy(MyPolicy).scope(MyScope)` |
| Custom middleware | `.layer(MyLayer)` |
| Raw router | `TestApp::from_router(my_router)` |
| Build model in-memory | `MyModel::factory().field(val).build()` |
| Persist model to DB | `MyModel::factory().field(val).create(&pool).await` |
| Pin time (no advance) | `.with_clock(FixedClock::at(dt))` |
| Advance time in test | `.with_clock(TickingClock::starting_at(dt))` + `client.advance_clock(dur)` |

---

## Testing time-sensitive logic

Autumn ships a first-class `Clock` extractor so that handlers read wall-clock
time through a swappable interface rather than calling `chrono::Utc::now()`
directly. In tests you pin or advance time with
`TestApp::with_clock` and `TestClient::advance_clock` — no `sleep`, no Tokio
timer games, no third-party crates.

### The Clock extractor

Declare `clock: Clock` as a handler argument to access the framework clock:

```rust
use autumn_web::prelude::*;
use autumn_web::time::Clock;

#[get("/token-age")]
async fn token_age(clock: Clock) -> String {
    format!("current time: {}", clock.now())
}
```

If no custom clock is configured, `Clock` delegates to `chrono::Utc::now()`
with zero overhead — existing handlers that don't take `Clock` keep working
unchanged.

### FixedClock — pin time to a known instant

Use [`FixedClock`] when you need a stable reference time but don't need to
advance it between requests:

```rust
use autumn_web::test::TestApp;
use autumn_web::time::FixedClock;
use chrono::{TimeZone, Utc};

#[tokio::test]
async fn token_is_fresh_at_issue_time() {
    let issued_at = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
    let client = TestApp::new()
        .routes(routes![token_age])
        .with_clock(FixedClock::at(issued_at))
        .build();

    client.get("/token-age").send().await.assert_ok();
}
```

### TickingClock — advance time between requests

Use [`TickingClock`] when the test needs to step time forward. Pass it to
`with_clock` and call `advance_clock` on the built client between requests:

```rust
use autumn_web::test::TestApp;
use autumn_web::time::{Clock, TickingClock};
use chrono::{TimeZone, Utc};
use std::time::Duration;

#[get("/token")]
async fn check_token(clock: Clock) -> axum::http::StatusCode {
    let issued = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
    if clock.now() < issued + chrono::Duration::days(30) {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::UNAUTHORIZED
    }
}

#[tokio::test]
async fn token_expires_after_30_days() {
    let issued = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
    let client = TestApp::new()
        .routes(routes![check_token])
        .with_clock(TickingClock::starting_at(issued))
        .build();

    client.get("/token").send().await.assert_status(200); // valid at issue

    client.advance_clock(Duration::from_secs(29 * 24 * 3600));
    client.get("/token").send().await.assert_status(200); // still valid

    client.advance_clock(Duration::from_secs(2 * 24 * 3600));
    client.get("/token").send().await.assert_status(401); // expired!
}
```

This test runs in **< 10 ms** — no `sleep`, no Tokio time games, no
third-party mocking crates. Compare that to a sleep-based equivalent that
would have to wait at least 30 real days (or use a very short hard-coded TTL
that doesn't match production).

### Sharing a TickingClock handle

`TickingClock` is `Clone`. Cloning shares the same internal instant so you can
keep a handle in the test and still wire the same clock into the app:

```rust
let clock = TickingClock::starting_at(Utc::now());
let client = TestApp::new()
    .with_clock(clock.clone())  // shares state with `clock`
    .build();

clock.advance(Duration::from_secs(3600)); // or: client.advance_clock(...)
```

Both `clock.advance(...)` and `client.advance_clock(...)` advance the same
shared instant — use whichever is more ergonomic for your test.

### Signed-URL and scheduler determinism

The same `clock_unix_secs` helper that handlers use is also available for
framework internals:

```rust
use autumn_web::time::{TickingClock, clock_unix_secs};
use autumn_web::storage::local::{SigningKey, sign, verify_with_now};

let clock = TickingClock::starting_at(Utc::now());
let key = SigningKey::new(b"my-signing-key".to_vec());
let blob_key = "uploads/file.png";

let now = clock_unix_secs(&clock);
let expires_at = now + 300; // 5 minutes
let sig = sign(key.as_bytes(), blob_key, expires_at);

// Advance past expiry without sleeping:
clock.advance(Duration::from_secs(400));
assert!(verify_with_now(key.as_bytes(), blob_key, expires_at, &sig, clock_unix_secs(&clock)).is_err());
```

### Comparison with other frameworks

| Framework | Time testing |
|-----------|--------------|
| Spring Boot | `java.time.Clock` injectable bean; `Clock.fixed(instant, zone)` in tests |
| Rails | `travel_to`, `freeze_time` from `ActiveSupport::Testing::TimeHelpers` |
| Django | `freezegun` (third-party) |
| Phoenix | Mox or callable indirection (third-party) |
| **Autumn** | `Clock` extractor + `with_clock` + `advance_clock` — first-class, no third-party crates |

[`FixedClock`]: https://docs.rs/autumn-web/latest/autumn_web/time/struct.FixedClock.html
[`TickingClock`]: https://docs.rs/autumn-web/latest/autumn_web/time/struct.TickingClock.html

---

*Next: [Tutorial Chapter 11 — Writing Tests](tutorial/11-testing.md)*
