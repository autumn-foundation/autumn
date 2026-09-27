# Content Negotiation

Some resources want two faces: a browser should get a rendered HTML page, while
a mobile app or CLI hitting the *same URL* wants JSON. Autumn's `Negotiate`
extractor lets one handler serve both from a single source of truth — no
duplicated route, no `if accept_header.contains("json")` branching by hand.

`Negotiate` reads the request's `Accept` header, and its
[`respond`](#the-respond-method) method takes a Maud closure for the HTML branch
and a `Serialize` value for the JSON branch. Only the chosen branch is
materialized: the HTML closure never runs for a JSON response, and the JSON value
is never serialized for an HTML response. The result carries `Vary: Accept` so
shared caches key the two representations separately.

`Negotiate`, its `Negotiated` response, and the `Format` it resolves to all ship
in `autumn_web::prelude`, so `use autumn_web::prelude::*;` is all you need.

## One handler, two representations

```rust
use autumn_web::prelude::*;
use serde::Serialize;

/// The resource, serializable straight to the JSON body.
///
/// `Copy` so the HTML closure and the JSON value can each own a copy — the
/// closure captures one while the original is handed to `.respond`, avoiding a
/// move/borrow conflict over a single value. For a non-`Copy` payload, build the
/// markup from borrowed fields and `.clone()` the value into the JSON arm.
#[derive(Clone, Copy, Serialize)]
struct Widget {
    id: i64,
    weight_grams: i64,
}

#[get("/widgets/{id}")]
async fn show(negotiate: Negotiate, id: Path<i64>) -> impl IntoResponse {
    let widget = Widget {
        id: *id,
        weight_grams: 240,
    };

    negotiate.respond(
        // Runs only for an HTML response.
        move || html! {
            h1 { "Widget #" (widget.id) }
            p { (widget.weight_grams) " g" }
        },
        // Serialized only for a JSON response.
        widget,
    )
}
```

Now the same route answers both audiences:

```text
$ curl -H 'Accept: text/html' localhost:3000/widgets/1
<h1>Widget #1</h1><p>240 g</p>

$ curl -H 'Accept: application/json' localhost:3000/widgets/1
{"id":1,"weight_grams":240}
```

A browser sends `Accept: text/html,...` and gets the page; an API client sends
`Accept: application/json` and gets the object. Both responses include
`Vary: Accept`.

## The `respond` method

`Negotiate::respond(html, json)` returns a `Negotiated` responder:

- `html: impl FnOnce() -> Markup` — a closure, so the markup is built lazily and
  only when HTML is chosen. Wrap any borrowed state in a `move` closure (or copy
  `Copy` data in) so it outlives the call.
- `json: impl Serialize` — any serializable value; it becomes the JSON body via
  `axum::Json`.

For the common browser-detail-page shape, `respond` is the whole API surface you
need. If you only want to *inspect* the negotiated choice without producing a
body, call `negotiate.format()`, which returns a `Format` (`Format::Html` or
`Format::Json`).

## How the `Accept` header is resolved

The choice follows RFC 7231 §5.3 content negotiation over the standard `Accept`
grammar, resolving each format's *effective* q-value from the most specific media
range that names it — `type/subtype` beats `type/*` beats `*/*`:

- **No preference** — a missing/empty `Accept`, a bare `*/*`, or a wildcard-only
  tie where neither side is named directly — serves the **default**, which is
  `Format::Html` (browser-first).
- **Higher effective q wins.** `Accept: application/json;q=0.9, text/html;q=1.0`
  serves HTML; `Accept: text/html;q=0.1, */*;q=1` serves JSON (the demoted
  `text/html` loses to JSON lifted by the `*/*;q=1` wildcard). On an exact tie,
  the earlier list entry wins.
- **`q=0` is an exclusion, not a demotion.** A format whose effective q is `0`
  is *forbidden* and never served — not via a wildcard and not via the default.
  `Accept: application/json;q=0` serves HTML even under a JSON default.
- **406 when everything is forbidden.** If the client forbids every
  representation the handler can produce (e.g.
  `Accept: text/html;q=0, application/json;q=0`, or a bare `*/*;q=0`), the
  responder answers `406 Not Acceptable` with a short plain-text body — and still
  sets `Vary: Accept`.

`application/problem+json` is deliberately **not** treated as the JSON success
tier: it is an error-path (Problem Details) signal, so a request that names only
`application/problem+json` leaves both HTML and JSON unmentioned and falls back
to the default, exactly like `application/xml` would.

## Overriding the default

When the client expresses no concrete preference, the default is HTML. For an
API-first resource where an anonymous `curl` should get JSON, flip it with
`default_format`:

```rust
#[get("/widgets/{id}")]
async fn show(negotiate: Negotiate, id: Path<i64>) -> impl IntoResponse {
    let widget = Widget { id: *id, weight_grams: 240 };

    negotiate
        .default_format(Format::Json)
        .respond(move || html! { h1 { "Widget #" (widget.id) } }, widget)
}
```

Now a bare `curl` (whose `Accept` is `*/*`) receives JSON, while a browser — which
explicitly asks for `text/html` — still gets the page. Note the default never
resurrects a *forbidden* format: `application/json;q=0` still serves HTML even
with a JSON default.

## Try it in the todo example

The [`todo-app` example](../../examples/todo-app) mounts a content-negotiated
`GET /todos/summary` route (in `src/routes/api.rs`) that returns the todo counts
as an HTML card to browsers and as a JSON object to API clients from one handler.
With the app running:

```bash
curl localhost:3000/todos/summary                                # HTML
curl -H 'Accept: application/json' localhost:3000/todos/summary   # JSON
```

## When *not* to use `Negotiate`

- **A pure JSON API** — just return `Json<T>`; there is no HTML branch to
  negotiate.
- **A pure HTML app** — return `Markup` directly.
- **htmx partials** — negotiate on the `HX-Request` header with the `HxRequest`
  extractor instead; that is a full-page-vs-fragment split, not a
  media-type split.

`Negotiate` earns its keep exactly when *one* resource legitimately has both an
HTML page and a JSON representation and you want a single handler to own both.
