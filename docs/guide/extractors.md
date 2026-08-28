# Extractors

An **extractor** is a handler parameter. Autumn builds each one from the
request before the handler body runs, so the handler signature *is* its
declaration of what the request must contain:

```rust,ignore
use autumn_web::prelude::*;

#[get("/r/{slug}/posts")]
async fn index(
    Path(slug): Path<String>,      // from the URL path
    Query(q): Query<PostFilter>,   // from the query string
    session: Session,              // from the session cookie
    mut db: Db,                    // from the connection pool
) -> AutumnResult<Markup> { /* … */ }
```

If an extractor cannot be built, the handler never runs and the framework
returns the extractor's own error — a 400 for a malformed query string or an
unparseable path segment, a 422 for a body that fails validation, a 401 from
`Auth` or `#[secured]` when a signed-in user is required. You do not write that
branch.

Two extractors that deliberately do *not* fail: `Session` is infallible — a
visitor with no session cookie gets a fresh anonymous one, so `Session` alone
never rejects a request and authentication is `Auth` / `#[secured]`'s job. And
404 is for a route or record that does not exist, which is a handler's
decision, not an extractor's.

This guide covers the extractor catalog, the two rules that govern ordering,
how to decode structured query strings, and how to write your own.

---

## The catalog

### Request data

| Extractor | Reads | Fails with |
|---|---|---|
| `Path<T>` | path parameters — `/users/{id}` | 400 when a segment will not parse into `T` |
| `Query<T>` | the query string, including sequences and nested objects | 400, naming the field path |
| `Form<T>` | a `application/x-www-form-urlencoded` body | 400 / 415 |
| `Json<T>` | a JSON body — and doubles as a JSON **response** type | 400 / 415 / 422 |
| `Multipart` | a `multipart/form-data` body, with the app's upload policy applied | 400, or 413 past `security.upload.max_request_size_bytes` |
| `ChangesetForm<T>` | a form body **plus** validation errors and the CSRF token | 415 / 400 on an undecodable body — but *never* on a validation failure |
| `Valid<Inner>` | wraps `Json`/`Form`/`Query` and validates the result | 422 with field-level details |
| `CurrentPath` | the request path, for nav highlighting | never |

### Identity and state

| Extractor | Reads |
|---|---|
| `Session` | the session store for this request |
| `Auth` | the authenticated principal |
| `ApiToken` / `RequireApiToken` | a scoped service token |
| `CsrfToken` | the token to embed in a form |
| `Consent` | the visitor's cookie-consent choice |
| `Tenant` | the resolved tenant for row-level multi-tenancy |
| `State<AppState>` | your application state |
| `Db`, `ShardedDb` | a pooled database connection |
| `Flash` | the flash-message store |
| `Flags`, `Experiments` | feature-flag and A/B assignments |
| `Events`, `Notifications` | the event bus and the notification store |
| `SeoMeta` | the route's `seo(...)` declaration, as a builder |
| `Negotiate` | the client's preferred response format |

Most of these are infallible: `Session`, `Consent`, `Flash`, and `CurrentPath`
cannot reject a request, so taking one is never a way to turn a caller away.
The exception is `CsrfToken`, and its failure is a **misconfiguration** rather
than a bad request — it returns 500 when `CsrfLayer` is not enabled, because a
handler asking for a token in an app that mints none is a setup bug, not
something a client did. Take `Option<CsrfToken>` (infallible) on a route that
must still render when CSRF is off.

### Client identity behind a proxy

Never read `X-Forwarded-*` yourself. These three resolve it once per request
through the operator's `[security.trusted_proxies]` policy:

| Extractor | Resolves |
|---|---|
| `ClientAddr` | the real client IP |
| `ClientHost` | the external host |
| `ClientScheme` | the external scheme |

### Pagination

| Extractor | Reads |
|---|---|
| `PageRequest` | `?page=&size=` — clamped, never rejected |
| `CursorRequest` | `?cursor=&size=` — an unreadable cursor means "first page" |
| `ListQuery` | `?sort=&dir=&filter[col]=` — allowlisted by the repository, never by the extractor |

See the [pagination guide](./pagination.md).

Anything Axum offers that Autumn does not re-export is still available under
`autumn_web::reexports::axum::extract`.

---

## Two ordering rules

**1. At most one body extractor, and it goes last.**

`Form`, `Json`, `Multipart`, `ChangesetForm`, and `Valid<…>` consume the
request body. Only one parameter can do that, and it must be the final
parameter — everything before it reads only the request *head*, which stays
available.

```rust,ignore
// ✅ body extractor last
async fn create(session: Session, mut db: Db, form: ChangesetForm<PostForm>) -> Response

// ❌ does not compile — `Session` cannot run after the body is consumed
async fn create(form: ChangesetForm<PostForm>, session: Session) -> Response
```

**2. Head extractors run left to right.**

That matters when one has a side effect or holds a resource. `Db` in
particular checks a connection out of the pool **eagerly**, at extraction, and
holds it until it is dropped:

```rust,ignore
async fn show(mut db: Db, repo: PgPostRepository) -> AutumnResult<Markup> {
    let post = load(&mut db).await?;
    drop(db);                       // release before the repository checks out its own
    let related = repo.recent().await?;
    // …
}
```

Holding two pooled connections in one handler is invisible in development and
fatal under load: with the default ten-connection pool, ten concurrent requests
each holding one and waiting for a second can never make progress. Drop the
first before acquiring the second.

---

## `Query<T>`: sequences and nested structures

`Query<T>` used to be strictly flat, because it delegated to
`serde_urlencoded`: it decoded `?q=foo&page=2` and nothing else. A
`Vec<String>` field fed `?tags=a&tags=b` failed with *invalid type: string
"a", expected a sequence*, and a nested struct field could not be expressed at
all. Builders worked around it with comma-separated strings and
JSON-in-a-string parameters.

Autumn 0.7 decodes the query string through
[`query_string`](https://docs.rs/autumn-web/latest/autumn_web/query_string/)
instead. It is a **superset**: a query of unique scalar keys decodes exactly as
before, and four more shapes now work.

### The wire format

```text
q=foo                       scalar
page=2                      scalar, coerced to the field's type
tags=a&tags=b               repeated key   → ["a", "b"]
tags[]=a&tags[]=b           append form    → ["a", "b"]
tags[0]=a&tags[2]=c         indexed form   → ["a", "c"]   (gaps compact)
filter[status]=open         nested object  → { status: "open" }
items[0][sku]=A-1           array of objects
```

Percent-encoded brackets (`%5B` / `%5D`) are the same thing — keys are
percent-decoded before parsing, so a client that encodes them round-trips
identically.

That bracket dialect is not invented here. It is the encoding
[`nested_form`](./nested-forms.md) already renders for repeated form rows,
generalized to arbitrary objects, sequences, and depths.

### A worked example

```rust,ignore
use autumn_web::prelude::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct Filter {
    status: Option<String>,
    min_score: Option<i64>,
}

#[derive(Deserialize)]
struct PostSearch {
    #[serde(default)]
    q: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    filter: Filter,
}

#[get("/search")]
async fn search(Query(params): Query<PostSearch>) -> AutumnResult<Markup> {
    // GET /search?q=rust&tags=web&tags=async&filter[status]=open&filter[min_score]=10
    //   params.q                 == "rust"
    //   params.tags              == ["web", "async"]
    //   params.filter.status     == Some("open")
    //   params.filter.min_score  == Some(10)
    render(&params)
}
```

### Semantics worth knowing

**Scalar coercion matches `serde_urlencoded`.** Values arrive as text and are
parsed into the field's type, so `page=2` fills a `u32` and `flag=true` fills a
`bool`. An `Option<T>` that is *present but empty* (`?page=`) still visits
`Some`; only an **absent** key is `None`.

**A duplicated key is an error in a single-valued position.** `?q=a&q=b`
against a `String` field is rejected. Quietly picking one of two values is how
parameter-pollution bugs are built. A **sequence** field takes every
occurrence — that is the point of the repeated-key form.

**Errors name the field, never the value.** A failure renders as
`filter.min_score: invalid digit found in string`. The submitted text is never
echoed, because that message goes into the 400 body and into your error
reporter, and a query parameter can hold a secret.

**A shape conflict poisons one key, not the request.**
`?filter=flat&filter[status]=open` uses one name as both a scalar and a
container. That key is rejected *if your type claims it*; a target that ignores
it — ad-tracking junk, a crawler's garbage parameter — still decodes, exactly
as it did when the key was merely unrecognized.

**Malformed brackets stay literal.** A key the grammar cannot parse
(`weird[unclosed`) is used verbatim as a flat key, so a stray bracket never
turns into a parse failure.

**Nesting is depth-capped** at `query_string::MAX_DEPTH` (16), and indices key
an ordered map rather than a `Vec`, so neither deep nesting nor a huge index
(`tags[4000000000]=x`) lets a request drive unbounded allocation.

**Map iteration is key-ordered.** Each level is keyed by a `BTreeMap`, so a
`Query<Vec<(String, String)>>` target sees pairs sorted by key rather than in
submission order. Occurrences of a single key keep their relative order.

### One upgrade note

Brackets are now *structure*, not part of the key text. A target that types a
parameter as a plain value — `Query<HashMap<String, String>>` is the common
one — used to receive `?filter[a]=1` as the literal key `"filter[a]"`. It now
sees a nested object and reports a decode error naming the fix: type the field
as a nested struct, or as `HashMap<String, serde_json::Value>` to accept either
shape.

### Bodies are unchanged

This is the **query string only**. `Form<T>` and
`NestedChangesetForm` still decode request *bodies* through `serde_urlencoded`
and the nested-row parser respectively. See [nested forms](./nested-forms.md).

### Why MCP tools care

A `Query<T>` on an MCP-exposed route becomes the tool's `query` object
property, and `tools/call` dispatch renders that object into this same wire
format. Before structured decoding, a tool whose `inputSchema` advertised
`tags: array` dispatched a request its own handler rejected. Now the contract
round-trips. See the [MCP guide](./mcp.md).

---

## Validation: `Valid<T>` and `ChangesetForm<T>`

Two extractors add validation, and they answer different questions.

`Valid<Inner>` wraps another extractor and **rejects** invalid input with a
422 carrying field-level details. Use it for API endpoints, where the client is
a program and an error response is the right answer:

```rust,ignore
#[post("/api/posts")]
async fn create(Valid(Json(post)): Valid<Json<NewPost>>) -> impl IntoResponse {
    // `post` is guaranteed valid
}
```

`ChangesetForm<T>` **never fails on a validation error**. It hands the handler
a changeset carrying both the submitted values and the per-field errors, so a
browser form can be re-rendered with the user's input still in the fields. (A
body that will not decode at all — the wrong `Content-Type`, a required field
missing — is still a hard rejection; only *invalid* input becomes data.) Use it
for server-rendered HTML:

```rust,ignore
#[post("/posts")]
async fn create(form: ChangesetForm<PostForm>) -> impl IntoResponse {
    match form.into_valid() {
        Ok(valid) => { /* persist, redirect */ }
        Err(form) => (StatusCode::UNPROCESSABLE_ENTITY, render(&form)).into_response(),
    }
}
```

The [forms and validation guide](./forms.md) covers both paths in full.

---

## Writing your own

Implement `FromRequestParts` for anything that reads only the request head, and
`FromRequest` for anything that consumes the body. A head extractor composes
freely with everything else; a body extractor must be the last parameter.

```rust,ignore
use autumn_web::prelude::*;
use autumn_web::reexports::axum::extract::FromRequestParts;
use autumn_web::reexports::http::request::Parts;

/// The requester's preferred locale, from `Accept-Language`.
pub struct PreferredLocale(pub String);

impl<S: Send + Sync> FromRequestParts<S> for PreferredLocale {
    type Rejection = AutumnError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let raw = parts
            .headers
            .get("accept-language")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("en");
        Ok(Self(raw.split(',').next().unwrap_or("en").trim().to_owned()))
    }
}
```

Three things to get right:

- **Choose the rejection deliberately.** Returning `AutumnError` puts your
  failure into the same Problem Details contract as every built-in extractor.
  Returning `Infallible` (as `PageRequest` and `ListQuery` do) means a
  malformed value falls back to a default instead of failing the request —
  correct when the parameter is a preference, wrong when it is an instruction.
- **Do not read `X-Forwarded-*`.** Use `ClientAddr` / `ClientHost` /
  `ClientScheme`, or read `ResolvedClientIdentity` from the request extensions
  if you are writing middleware.
- **Extract, do not authorize.** An extractor that silently drops records the
  caller may not see is an authorization rule hiding in a type. Put it in a
  [policy](./authorization.md), where `autumn routes audit` can prove it.

---

## Extractor or middleware?

If the logic hands a value to one handler, it is an extractor. If it must wrap
the request *and* the response, or short-circuit before the handler runs, it is
middleware. The [middleware guide](./middleware.md) opens with the full
decision table.

---

## See also

- [Forms, validation and normalization](./forms.md) — `ChangesetForm`, `Valid`,
  `#[normalize]`
- [Nested forms](./nested-forms.md) — the `items[0][sku]` encoding on the body side
- [Pagination](./pagination.md) — `PageRequest`, `CursorRequest`, `ListQuery`
- [Middleware](./middleware.md) — when to wrap the request instead
- [Content negotiation](./content-negotiation.md) — `Negotiate` and one handler,
  many formats
- [MCP tools](./mcp.md) — how `Query<T>` becomes a tool's argument schema
