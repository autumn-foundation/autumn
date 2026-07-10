---
name: autumn-patterns
description: >
  Use when writing, reviewing, or debugging Rust code in an autumn-web
  project; covers idiomatic patterns for testing, repositories, service
  layers, job design, security, error handling, and Maud templates that
  are not covered in the framework API reference.
---

# autumn-web — Idiomatic Patterns

Reference `skills/autumn-web/SKILL.md` for the API quick guide. This file
covers patterns and design decisions that apply across an Autumn app.

**First rule: prefer the framework idiom.** Before hand-rolling validation,
CRUD queries, pagination, forms, auth checks, background threads, or caching,
check the "Prefer framework idioms over raw Diesel/Axum" table in
`skills/autumn-web/SKILL.md`. Raw Axum/Diesel is allowed but is a last resort.

## Status fields: `#[state_machine]`, not hand-rolled checks

When a model has a status/phase column with legal transitions, declare them
with `#[state_machine(transitions(...))]` on the field and enforce with the
generated `transition_{field}_to` in `before_update` — never write your own
`match (old, new)` validation in hooks or handlers. See
`docs/guide/state-machines.md` and the worked example in
`skills/autumn-web/references/examples.md`.

## Testing with TestApp and TestClient

The `test-support` feature ships an in-process test client. No running
server is needed. Add the feature for tests only:

```toml
[dev-dependencies]
autumn-web = { version = "0.5", features = ["test-support"] }
```

```rust
use autumn_web::test::TestApp;  // not in prelude; requires test-support feature

#[tokio::test]
async fn create_post_returns_redirect() {
    let client = TestApp::new()
        .routes(routes![posts::create])
        .build();  // synchronous; returns TestClient

    let res = client
        .post("/posts")
        .form("title=Hello&body=World")  // form() takes a URL-encoded &str
        .send()
        .await;

    res.assert_status(302);
    // header() returns Option<&str>
    assert!(res.header("location").is_some_and(|loc| loc.contains("/posts/")));
}
```

Always use `TestApp` in tests — never spin up a real server or hit a live
database in unit tests.

### Assert a route's query budget — catch N+1 (trunk-dev)

Pin the SQL a route is allowed to run so an accidental N+1 fails the test:

```rust
client.get("/posts").send().await
    .assert_status(200)
    .assert_max_queries(3);   // panics naming the route if the count exceeds 3
```

`TestResponse::query_count()` returns the observed count (from the
`Server-Timing` query counter); `assert_max_queries(n)` chains and returns
`&Self` (issue #1262).

### Fake-data factories and bulk seeding (trunk-dev)

Don't hand-build model fixtures. `#[model]` generates a `{Model}Factory`; fill
the rest with realistic fake data inferred from each field's name + type:

```rust
let post = Post::factory().title("Fixed").fake().build();  // NewPost, unset fields faked
let posts = Post::factory().fake().build_many(10);         // Vec<NewPost>, each row re-drawn
let saved = Post::factory().fake().create(&mut db).await?; // persist
```

Generators live in `autumn_web::fake` (`name`, `email`, `sentence`,
`int_range`, `uuid`, …); seed deterministically with `AUTUMN_FAKE_SEED` or
`autumn_web::fake::reseed(seed)`. For bulk data outside tests,
`autumn seed --count N --model <Name>` (both flags together) inserts N faked rows
via the model's factory (issue #1343). See `docs/guide/seeding.md`.

## Repository design

Prefer free functions over a repository struct unless `#[repository]` is
generating a REST API. Repository structs are heavyweight; functions compose
better in tests.

```rust
// Good: free functions
pub async fn find_post(conn: &mut AsyncPgConnection, post_id: i64) -> AutumnResult<Post> {
    use crate::schema::posts::dsl::*;
    posts.find(post_id)  // generated PK column is `id`; use .find() for primary key lookups
        .first(conn)
        .await
        .map_err(|_| AutumnError::not_found_msg("post not found"))
}

// Use #[autumn_web::repository] when you need a generated REST API
#[autumn_web::repository(Post, api = "/api/posts", policy = PostPolicy, scope = PostScope)]
pub trait PostRepository {}
```

Repository-generated REST APIs must declare a `policy` in production or set
`security.allow_unauthorized_repository_api = true` explicitly — the default
will fail `autumn doctor --strict`.

## Service layer

Only add a service layer when logic is shared across multiple handlers or
jobs. For single-handler logic, keep it in the handler.

```rust
pub struct PostService<'a> {
    conn: &'a mut AsyncPgConnection,
}

impl<'a> PostService<'a> {
    pub fn new(conn: &'a mut AsyncPgConnection) -> Self { Self { conn } }

    pub async fn publish(&mut self, post_id: i64) -> AutumnResult<Post> {
        // business logic here
    }
}

// In the handler:
#[post("/posts/{id}/publish")]
#[secured]
async fn publish_post(Path(id): Path<i64>, mut db: Db) -> AutumnResult<Redirect> {
    let mut svc = PostService::new(&mut *db);
    svc.publish(id).await?;
    Ok(Redirect::to(&format!("/posts/{}", id)))
}
```

## Job design

Use `#[job]` for request-triggered work with retries. Keep jobs idempotent —
they may run more than once.

The `#[job]` macro takes `AppState` and a typed args struct (serializable).
The macro generates a `PascalCaseJob` struct with a static `enqueue` method.
On trunk-dev (unreleased — not in published 0.5.0) a job may also accept an
optional third `JobContext` argument for progress reporting on tracked jobs,
and a `queue = "name"` attribute for named priority queues.

Published 0.5.0 also supports idempotency/backpressure attributes:
`#[job(unique)]`, `unique_by = "field"`, `unique_for_ms = N`,
`concurrency = N`, `concurrency_key = "field"` — prefer these over
hand-rolled dedupe tables or semaphores.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendWelcomeEmailArgs {
    pub user_id: i64,
}

#[job(name = "send_welcome_email", max_attempts = 3, backoff_ms = 500)]
pub async fn send_welcome_email(state: AppState, args: SendWelcomeEmailArgs) -> AutumnResult<()> {
    let pool = state.pool().ok_or_else(|| AutumnError::service_unavailable_msg("no db"))?;
    let mut conn = pool.get().await.map_err(AutumnError::from)?;
    let user = find_user(&mut conn, args.user_id).await?;
    // state.extension::<Mailer>() returns Option<Arc<Mailer>>
    let mailer = state.extension::<Mailer>()
        .ok_or_else(|| AutumnError::service_unavailable_msg("mailer not configured"))?;
    // generated helpers take &Mailer — deref the Arc:
    UserMailer.send_welcome(&*mailer, user.email.clone(), user.username.clone()).await?;
    Ok(())
}

// Enqueue from a handler (generated struct name = PascalCase + "Job"):
SendWelcomeEmailJob::enqueue(SendWelcomeEmailArgs { user_id: user.id }).await?;
```

Use `#[scheduled]` for recurring work. Use `#[task]` for operator-invoked
CLI work (`autumn task <name>`).

For durable multi-step workflows or jobs that need activity retries, timers,
or human approval steps, reach for Autumn Harvest.

## Security checklist

Before shipping any route:

- [ ] `#[secured]` on any route that requires authentication
- [ ] `#[secured("admin")]` for admin-only routes
- [ ] `#[authorize("update", resource = Post)]` for record-level auth
- [ ] CSRF hidden field in every `<form method="POST">`
- [ ] `Valid<Form<T>>` or `Valid<Json<T>>` on every mutation
- [ ] No `unwrap()` — use `?` with `AutumnResult<T>`
- [ ] Secrets in env vars, not in `autumn.toml`

```rust
// Every POST form must include the CSRF token.
// `_csrf` is the default field name; if you set `security.csrf.form_field`
// in autumn.toml, use that value instead (or read it via CsrfConfig).
#[get("/posts/new")]
async fn new_post(csrf: CsrfToken) -> Markup {
    html! {
        form method="POST" action="/posts" {
            input type="hidden" name="_csrf" value=(csrf.token());
            // ...
        }
    }
}
```

In production, generate a stable signing secret before first deploy:
```bash
export AUTUMN_SECURITY__SIGNING_SECRET="$(openssl rand -hex 32)"
```

## Error handling idioms

Always return `AutumnResult<T>`. Use typed constructors — avoid rolling
custom status codes.

```rust
// Not found
.map_err(|_| AutumnError::not_found_msg("post not found"))?

// Validation failure (re-render form)
return Err(AutumnError::unprocessable_msg("title is too short"));

// Authorization failure
return Err(AutumnError::forbidden_msg("not your post"));
```

JSON clients receive RFC 7807 Problem Details automatically. HTML clients
get the configured error page renderer.

## Maud template patterns

Keep layout in a shared function. Avoid duplicating `DOCTYPE`, `<head>`, and
`<nav>` across templates.

```rust
pub fn layout(title: &str, content: Markup) -> Markup {
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { (title) " — MyApp" }
                script src=(HTMX_JS_PATH) defer {}
                script src=(HTMX_CSRF_JS_PATH) defer {}
                link rel="stylesheet" href="/static/css/autumn.css";
            }
            body {
                header { nav { /* ... */ } }
                main class="container mx-auto px-4 py-8" { (content) }
            }
        }
    }
}

pub fn post_show(post: &Post) -> Markup {
    layout(&post.title, html! {
        article {
            h1 { (post.title) }
            div class="prose" { (post.body) }
        }
    })
}
```

## Configuration layering

Config resolves lowest-to-highest: framework defaults → profile smart
defaults → `autumn.toml` → `[profile.<name>]` → `autumn-{profile}.toml` →
`AUTUMN_*` env vars.

Env var format: `AUTUMN_SECTION__FIELD`, double underscore as separator.
Examples: `AUTUMN_DATABASE__PRIMARY_URL`, `AUTUMN_JOBS__BACKEND`,
`AUTUMN_SECURITY__SIGNING_SECRET`.

Profile selection: `AUTUMN_ENV` > `AUTUMN_PROFILE` > `--profile` flag >
debug/release auto-detection.

## Crate naming reference

| Concept | Name |
|---|---|
| Library crate (crates.io) | `autumn-web` |
| Rust import path | `autumn_web::` |
| Entry macro | `#[autumn_web::main]` |
| CLI binary | `autumn` |
| Proc macros | `autumn-macros` |
| Admin plugin | `autumn-admin-plugin` |
| S3 storage | `autumn-storage-s3` |
| Redis cache | `autumn-cache-redis` |
