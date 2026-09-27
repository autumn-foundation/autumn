# Dev Request Inspector

The Autumn request inspector gives you a real-time view of every HTTP request
your application handled — method, path, status, duration, and the SQL queries
that fired — without leaving the browser or switching to a log terminal.

When running in `dev` profile, the inspector is automatically mounted at
`/_autumn/inspect`. Just make some requests, then open that URL.

---

## What the inspector shows

### Request list (`/_autumn/inspect`)

A table of the last *N* requests (default 100), newest first:

| Column  | Description |
|---------|-------------|
| Method  | HTTP verb (`GET`, `POST`, …) |
| Path    | Request path |
| Status  | HTTP status code (4xx/5xx highlighted) |
| Duration | Total wall time in milliseconds |
| Queries | Number of SQL queries recorded via `RequestInspector` |
| N+1?   | ⚠ badge when an N+1 pattern was detected |

Clicking any row opens the detail view.

### Request detail (`/_autumn/inspect/requests/{id}`)

- **Summary bar** — status, duration, Content-Type, Content-Length.
- **N+1 banner** — shown when the detector fired; includes the offending SQL
  template and how many times it appeared.
- **SQL query table** — lists every query recorded by `RequestInspector`
  with its execution time and the call site (`file:line`).
- **curl snippet** — a one-liner to reproduce the request from the terminal.

---

## Configuration

Add a `[dev]` block to `autumn.toml` to override any default:

```toml
[dev]
# Mount path for the inspector UI (default: "/_autumn/inspect")
inspector_path = "/_autumn/inspect"

# How many requests to keep in the ring buffer (default: 100, 0 = disable)
inspector_capacity = 200

# How many identical SQL statements must appear before an N+1 warning fires
# (default: 5, 0 = disable N+1 detection)
inspector_n_plus_one_threshold = 3
```

These settings are **ignored outside the `dev` profile**.

---

## N+1 detection

The detector normalises each SQL string (collapses whitespace, lower-cases)
and counts occurrences per request. If any template appears ≥ M times the
request is flagged.

**Example** — a hand-rolled loop that triggers N+1:

```rust
#[get("/posts")]
async fn index(db: Db, inspector: RequestInspector) -> Result<Html<String>, AutumnError> {
    let posts = Post::all(&mut db.primary().await?).await?;
    let mut results = Vec::new();
    for post in &posts {
        // ← This fires one SELECT per post — classic N+1
        let author = User::find(&mut db.primary().await?, post.author_id).await?;
        inspector.record_query(QueryRecord {
            sql: format!("SELECT * FROM users WHERE id = {}", post.author_id),
            params: vec![post.author_id.to_string()],
            elapsed_ms: 1,
            location: format!("{}:{}", file!(), line!()),
        });
        results.push((post, author));
    }
    Ok(Html(render_posts(&results)))
}
```

After hitting `/posts`, visit `/_autumn/inspect`. The row for the request will
show a ⚠ N+1 badge. Click it for the detail view, which shows the offending
SQL and the call site.

**Fixing the N+1** — replace the loop with a JOIN query. The N+1 badge
disappears from the next request recorded.

### Limitations

The detector flags *structural repetition*, not *semantic equivalence*. Some
patterns that are not true N+1s may be flagged:

- Intentional fan-out (e.g. checking 10 separate cache keys with the same
  template but different parameters)
- Batch operations that internally repeat a template

False positives are acceptable in `dev`: the goal is to surface likely
problems, not to be exhaustive. Adjust `inspector_n_plus_one_threshold` to
tune the sensitivity.

---

## Using `RequestInspector` in handlers and tests

The `RequestInspector` extractor lets handlers append SQL query records, and
lets integration tests assert query counts without the UI:

```rust
use autumn_web::inspector::{RequestInspector, QueryRecord};

#[get("/posts")]
async fn index(db: Db, inspector: RequestInspector) -> &'static str {
    // ... run queries ...
    inspector.record_query(QueryRecord {
        sql: "SELECT * FROM posts".to_owned(),
        params: vec![],
        elapsed_ms: 3,
        location: format!("{}:{}", file!(), line!()),
    });
    "ok"
}
```

In an integration test:

```rust
#[tokio::test]
async fn posts_index_issues_one_query() {
    use autumn_web::inspector::{InspectorBuffer, InspectorLayer};

    let buf = InspectorBuffer::new(10);
    let layer = InspectorLayer::new(buf.clone(), 5, "/_autumn/inspect".to_owned());
    let app = /* build your test router */axum::Router::new()
        .route("/posts", axum::routing::get(index))
        .layer(layer);

    let req = axum::http::Request::builder()
        .uri("/posts")
        .body(axum::body::Body::empty())
        .unwrap();
    let _ = tower::ServiceExt::oneshot(app, req).await.unwrap();

    let record = &buf.snapshot()[0];
    assert_eq!(record.query_count(), 1, "expected exactly 1 query");
    assert!(record.n_plus_one.is_none(), "no N+1 should be detected");
}
```

---

## Production safety

- The inspector **only mounts** when `profile = "dev"`. In `prod` and `test`
  profiles the `/_autumn/inspect` path does not exist (the router never adds
  the routes or the middleware).
- The ring buffer is **in-memory only** — no disk writes, no cross-process
  sharing. Each `autumn dev` worker has its own buffer.
- When the inspector is not mounted its overhead is **zero** — no middleware
  is applied to the production router.

> **⚠ Warning** — if you paste production-shaped query parameters into the
> inspector's parameter display, you are putting PII in your browser. The
> inspector renders query parameters verbatim in `dev` (PII filtering will
> be addressed when #697 lands).

---

## Disabling the inspector in dev

Set `inspector_capacity = 0` in `[dev]` to stop recording requests while
keeping the middleware absent overhead:

```toml
[dev]
inspector_capacity = 0
```

Or simply run in `test` or `prod` profile — the inspector is not mounted.

---

## The build-time counterpart

The inspector flags an N+1 only on a path you actually browse.
[`#[query_budget(N)]`](query-budgets.md) bounds a route's query count at
**compile time**, so a regression on an untested branch fails the build instead
of waiting to be clicked. Use both: the gate proves the upper bound, the
inspector shows you the SQL.

