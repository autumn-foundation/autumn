# Chapter 8: JSON API

**Goal:** By the end of this chapter, you will have a JSON API at `/api/todos`
that supports listing and creating todos, testable with curl or any HTTP
client.

---

## Sections

### `Json<T>` as Request and Response

`Json<T>` serves double duty in Autumn: it extracts a JSON request body
(when used as a handler parameter) and serializes a response to JSON with the
correct `Content-Type` header (when used as a return type).

### Creating `src/routes/api.rs`

Adding a new route module for JSON endpoints alongside the HTML routes.
Registering the new routes in `main.rs`.

### GET `/api/todos` — List as JSON

Returning `AutumnResult<Json<Vec<Todo>>>`. The same query as the HTML list
handler, different response format.

### POST `/api/todos` — Create from JSON

Accepting `Json<NewTodo>` as input, inserting into the database, and
returning the created `Todo` with `returning()` and `get_result()`.

### Sharing Logic Between HTML and JSON Handlers

When to share query code between formats and when to keep them separate.
Practical patterns for code reuse.

### Testing with curl

Example curl commands for listing and creating todos through the JSON API.

### Checkpoint

Expected project state with both HTML and JSON routes.

---

> **Not written yet.** This chapter's narrative doesn't exist yet. For the
> actual JSON API this chapter's goal describes, see
> [`examples/todo-app/src/routes/api.rs`](../../../examples/todo-app/src/routes/api.rs)
> — that's the reference implementation this tutorial builds toward. Its
> `list_json`/`create_json` handlers are declared as `#[get("/todos")]` /
> `#[post("/todos")]` but mounted at `/api/todos` via `.scoped("/api", ...)`
> in `main.rs` (see the `.scoped(...)` call there), matching this chapter's
> goal — but that scope also requires a bearer token
> (`POST /api/tokens` first), which is more than this chapter's "testable
> with curl" goal implies; drop the `RequireApiToken` extractor for an
> unauthenticated version instead if you don't want that step yet. The
> Getting Started guide's
> ["Query the database"](../getting-started.md#query-the-database) section
> shows the same `Json<T>` request/response pattern without the token
> layer. Continue to
> [Chapter 9 — Error Handling](09-errors.md) once yours responds to curl.

---

Previous: [Chapter 7 — Interactivity with htmx](07-htmx.md) | Next: [Chapter 9 — Error Handling](09-errors.md)
