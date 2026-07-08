# What Happens When...

The questions you have at hour two, not hour one. This guide covers the failure
modes, edge cases, and "what if" scenarios that the getting-started guide
doesn't cover.

---

## What Happens When a Database Query Fails?

### Short answer

The `?` operator converts the Diesel error into an `AutumnError` with HTTP
500, and Autumn returns a JSON error response.

### The full chain

```rust
#[get("/todos")]
async fn list(mut db: Db) -> AutumnResult<Json<Vec<Todo>>> {
    let todos = todos::table.load(&mut *db).await?; // <-- fails here
    Ok(Json(todos))
}
```

1. Diesel returns `Err(diesel::result::Error::...)` (connection lost, syntax
   error, constraint violation, etc.)
2. The `?` operator hits the blanket `From<E: Error> for AutumnError` impl,
   which wraps it as a 500
3. `AutumnError` implements `IntoResponse`, producing Problem Details JSON:
   ```json
   {
     "type": "https://autumn.dev/problems/internal-server-error",
     "title": "Internal Server Error",
     "status": 500,
     "detail": "Internal server error",
     "instance": "/todos",
     "code": "autumn.internal_server_error",
     "request_id": "018f4f30-6b7c-4b4c-8dc0-70a2c8d7f97d",
     "errors": []
   }
   ```
4. In production, 5xx `detail` is client-safe; use `request_id` to find the
   full operator-facing cause in logs. In development, `detail` includes the
   original diagnostic message.

### Refining the status code

For expected failures (like "record not found"), map the error:

```rust
#[get("/todos/{id}")]
async fn get_one(Path(id): Path<i32>, mut db: Db) -> AutumnResult<Json<Todo>> {
    let todo = todos::table
        .find(id)
        .first(&mut *db)
        .await
        .map_err(AutumnError::not_found)?;  // 404 instead of 500

    Ok(Json(todo))
}
```

Or use the string convenience method:

```rust
.map_err(|_| AutumnError::not_found_msg("todo not found"))?;
```

### Connection pool exhaustion

If all connections are in use and the pool times out, `Db` extraction fails
with a 503 Service Unavailable before your handler even runs. The log shows:

```
ERROR autumn: Failed to acquire database connection: pool timeout
```

---

## What Happens When Config Is Missing?

### No `autumn.toml` file at all

Not an error. Autumn uses its compiled-in defaults:

| Setting       | Default value     |
|---------------|-------------------|
| `server.port` | 3000              |
| `server.host` | 127.0.0.1         |
| `log.level`   | info              |
| `log.format`  | Auto              |
| `database.primary_url` / `database.url` | (none -- no DB) |

### No `[database]` section (or no primary URL)

Autumn starts without a database pool. Handlers that inject `Db` return 503:

```
  INFO autumn: Database not configured
```

```json
{
  "type": "https://autumn.dev/problems/service-unavailable",
  "title": "Service Unavailable",
  "status": 503,
  "detail": "Database not configured",
  "instance": "/todos",
  "code": "autumn.service_unavailable",
  "request_id": "018f4f30-6b7c-4b4c-8dc0-70a2c8d7f97d",
  "errors": []
}
```

This is intentional -- static sites and API gateways don't need a database.

### Invalid TOML syntax

Startup fails immediately with a clear parse error:

```
Failed to load configuration: TOML parse error at line 3, column 5
```

### Unknown or misspelled profile

Autumn logs a warning with a "did you mean?" suggestion (Levenshtein distance
matching) and falls back to defaults:

```
WARN autumn: Unknown profile "dvv", did you mean "dev"?
```

### Invalid database URL scheme

Configuration validation catches it immediately (both `postgres://` URLs and
libpq-style keyword/value strings like `host=db user=app sslmode=require` are
accepted; anything else is rejected):

```
Failed to load configuration: Invalid database.primary_url: must start with postgres:// or postgresql://, or be a keyword/value connection string
```

### Environment variable overrides

Every config field can be overridden via `AUTUMN_SECTION__FIELD` (double
underscore). These always win over TOML files:

```bash
AUTUMN_SERVER__PORT=8080 AUTUMN_LOG__LEVEL=debug cargo run
```

---

## What Happens When You Need a Custom Extractor?

### The pattern

Implement `FromRequestParts<AppState>` and return `AutumnError` on failure:

```rust
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use autumn_web::prelude::*;

pub struct ApiKey(pub String);

impl FromRequestParts<AppState> for ApiKey {
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let key = parts
            .headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| AutumnError::unauthorized_msg("missing API key"))?;

        Ok(ApiKey(key.to_owned()))
    }
}
```

### Using it in handlers

Just add it to the parameter list -- Axum calls your extractor automatically:

```rust
#[get("/protected")]
async fn protected(key: ApiKey) -> String {
    format!("Authenticated with key: {}", key.0)
}
```

### What happens when it fails

Your `Rejection` type (`AutumnError`) is returned directly as the HTTP
response. The handler never runs. The request short-circuits with whatever
status code you chose in the extractor.

### Built-in extractors

| Extractor     | Source                   | Failure mode          |
|---------------|--------------------------|-----------------------|
| `Db`          | Connection pool          | 503 if no DB or pool exhausted |
| `Auth<T>`     | Request extensions       | 401 if not authenticated |
| `Session`     | Session store            | 500 if session backend fails |
| `Valid<Json<T>>` | Request body + validation | 422 with field-level errors |
| `CsrfToken`   | Session                  | 500 if session unavailable |
| `Path<T>`     | URL path segments        | 400 if parse fails   |
| `Query<T>`    | URL query string         | 400 if parse fails   |
| `Json<T>`     | Request body             | 400/422 if parse fails |
| `Form<T>`     | Form-encoded body        | 400/422 if parse fails |

---

## What Happens When Validation Fails?

### Using the `Valid<T>` extractor

```rust
use autumn_web::prelude::*;
use validator::Validate;

#[derive(Deserialize, Validate)]
struct NewPost {
    #[validate(length(min = 1, max = 200))]
    title: String,
    #[validate(length(min = 10))]
    body: String,
}

#[post("/posts")]
async fn create(Valid(Json(post)): Valid<Json<NewPost>>) -> &'static str {
    // Only reached if validation passes
    "created"
}
```

If validation fails, your handler never runs. The response is:

```json
{
  "type": "https://autumn.dev/problems/validation-failed",
  "title": "Validation Failed",
  "status": 422,
  "detail": "Validation failed",
  "instance": "/posts",
  "code": "autumn.validation_failed",
  "request_id": "018f4f30-6b7c-4b4c-8dc0-70a2c8d7f97d",
  "errors": [
    { "field": "title", "messages": ["length must be between 1 and 200"] },
    { "field": "body", "messages": ["length must be at least 10"] }
  ]
}
```

### Manual validation

Build validation errors yourself:

```rust
use std::collections::HashMap;

let mut errors = HashMap::new();
errors.insert("email".into(), vec!["already taken".into()]);
return Err(AutumnError::validation(errors));
```

---

## What Happens When the Server Shuts Down?

Autumn handles `SIGTERM` and `Ctrl+C` with graceful shutdown:

1. Stop accepting new connections
2. Wait for in-flight requests to complete (up to `shutdown_timeout_secs`)
3. Stop scheduled tasks
4. Log: `"Server shut down cleanly"`

The timeout is profile-aware:
- **dev**: 1 second (fast restart during development)
- **prod**: 30 seconds (drain in-flight requests)

If in-flight requests don't finish before the timeout, they are dropped.

---

## What Happens When a Scheduled Task Fails?

```rust
#[scheduled(every = "5m")]
async fn cleanup(state: AppState) -> AutumnResult<()> {
    // If this returns Err, the error is logged and the task runs again
    // at the next scheduled interval. It does NOT crash the server.
    Ok(())
}
```

- **Error**: Logged at `ERROR` level. The task is rescheduled normally.
- **Panic**: Caught by the Tokio runtime. Logged. Task continues on next tick.
- **The server keeps running** regardless of task failures.

---

## What Happens When You Return the Wrong Type?

Handlers can return anything that implements Axum's `IntoResponse`. If you
return a type that doesn't implement it, you get a compile-time error -- not a
runtime error.

Common return types:

| Type                  | Response                                |
|-----------------------|-----------------------------------------|
| `&str` / `String`    | 200, `text/plain`                       |
| `Json<T>`            | 200, `application/json`                 |
| `Markup` (Maud)      | 200, `text/html`                        |
| `AutumnResult<T>`    | `T` on success, JSON error on failure   |
| `(StatusCode, T)`    | Custom status code + body               |
| `Response`           | Full control                            |

---

## What Happens When Two Routes Conflict?

If you register the same path twice:

```rust
.routes(routes![handler_a])  // #[get("/foo")]
.routes(routes![handler_b])  // #[get("/foo")]
```

The last one wins. Axum processes routes in order and uses the first match
for a given method+path, but Autumn builds the router additively. If you
need different behavior on the same path, use different HTTP methods.

---

## What Happens When a Migration Fails?

Auto-migration runs at startup (dev profile only by default). If a migration
fails:

1. The error is logged
2. The server exits with code 1
3. You see the SQL error in the output

Fix the migration SQL and restart. Migrations are transactional -- a failed
migration is rolled back.

---

## What Happens When the Database Is Down at Startup?

Pool creation succeeds (it's lazy -- pools don't connect immediately). The
first handler that uses `Db` will fail with 503 when it tries to acquire a
connection:

```json
{
  "type": "https://autumn.dev/problems/service-unavailable",
  "title": "Service Unavailable",
  "status": 503,
  "detail": "connection refused",
  "instance": "/todos",
  "code": "autumn.service_unavailable",
  "request_id": "018f4f30-6b7c-4b4c-8dc0-70a2c8d7f97d",
  "errors": []
}
```

The health endpoint at `/health` will still respond (it doesn't require a DB
connection to serve basic status). If `health.detailed` is enabled, the
health check will report the database as unhealthy.

---

## What Happens When You Use `Db` Without a Database?

If neither `database.primary_url` nor the legacy `database.url` is configured,
the primary/write pool is `None`. The `Db` extractor returns 503 immediately:

```json
{
  "type": "https://autumn.dev/problems/service-unavailable",
  "title": "Service Unavailable",
  "status": 503,
  "detail": "Database not configured",
  "instance": "/todos",
  "code": "autumn.service_unavailable",
  "request_id": "018f4f30-6b7c-4b4c-8dc0-70a2c8d7f97d",
  "errors": []
}
```

Your handler code never executes. This means you can safely have database
handlers in your codebase and run without a database -- they'll just return
503 until you configure one.

---

## What Happens When CORS Is Misconfigured?

If `cors.allowed_origins` is empty (the default), CORS middleware is not
applied. No `Access-Control-*` headers are sent.

If origins are configured but a request comes from an unlisted origin, the
browser blocks the response (Autumn sends the response, but without the
required CORS headers, the browser rejects it).

Dev profile smart defaults set `allowed_origins = ["*"]` for convenience.
Prod defaults leave it empty -- you must explicitly configure allowed origins.

---

## What Happens When Session/Auth Isn't Set Up?

If you use `#[secured]` or the `Auth<T>` extractor without configuring session
middleware, the extractor will fail to find a user in request extensions and
return 401 Unauthorized.

If you use `Session` without a session store backend, session operations will
fail at runtime with a 500 error.

---

## Quick Reference: Error Status Codes

| Scenario                        | Status | Source             |
|---------------------------------|--------|--------------------|
| Unhandled error via `?`         | 500    | Blanket `From`     |
| `AutumnError::not_found(e)`     | 404    | Explicit           |
| `AutumnError::bad_request(e)`   | 400    | Explicit           |
| `AutumnError::unprocessable(e)` | 422    | Explicit           |
| `AutumnError::unauthorized(e)`  | 401    | Explicit           |
| `AutumnError::forbidden(e)`     | 403    | Explicit           |
| Validation failure (`Valid<T>`) | 422    | Extractor          |
| DB not configured (`Db`)        | 503    | Extractor          |
| Pool exhaustion (`Db`)          | 503    | Extractor          |
| Auth missing (`Auth<T>`)        | 401    | Extractor          |
| `#[secured]` auth check         | 401/403| Generated code     |
| Path/Query parse failure        | 400    | Axum extractor     |
