# Flash messages

Flash messages are one-shot notices that survive a redirect: set one before a
`303 See Other` (the "post-redirect-get" pattern) and it is shown exactly once on
the next page, then cleared. They are the idiomatic way to confirm a successful
form submission ("Post created") or report a failure ("Invalid credentials").

Flash is **on by default** — `use autumn_web::prelude::*;` exposes `Flash` in any
app built with the default features. There is no Cargo feature to enable and no
plumbing to write: the generators (`autumn generate scaffold`, `autumn generate
auth`) emit flash calls and render them in their layouts for you.

## Set a flash, then redirect

Inject the [`Flash`] extractor and call one of `success` / `info` / `warning` /
`error` before returning a redirect. Flash is backed by the signed session
cookie, so the message rides along to the next request.

```rust
use autumn_web::prelude::*;

#[post("/posts")]
async fn create(flash: Flash, mut db: Db, form: Form<NewPost>) -> AutumnResult<Redirect> {
    // ... insert the post ...
    flash.success("Post created.").await;
    Ok(Redirect::to("/posts"))
}
```

## Render in your layout — one line

On the page that follows the redirect, call `flash.render().await` and splice the
result into your layout. `render()` consumes all pending messages (clearing them
so they never show twice) and returns the markup for them:

```rust
#[get("/posts")]
async fn index(flash: Flash) -> AutumnResult<Markup> {
    Ok(layout("Posts", flash.render().await, html! {
        h1 { "Posts" }
        // ...
    }))
}

fn layout(title: &str, flash: Markup, content: Markup) -> Markup {
    html! {
        // ...
        body {
            (flash)      // renders pending notices, or nothing
            (content)
        }
    }
}
```

`render()` requires the `maud` feature (part of the default feature set) and
always emits a stable `<div id="flash">` container — even when there are no
messages — so it can double as a target for htmx out-of-band swaps (see below).
Each message carries a `flash flash-<level>` class (`flash-success`,
`flash-info`, `flash-warning`, `flash-error`).

### Styling

Default styling ships as a framework-served stylesheet. Link it once in your
layout's `<head>` (the generators do this for you):

```rust
link rel="stylesheet" href=(autumn_web::flash::FLASH_CSS_PATH);
```

It is served as a same-origin asset (not inline `style` attributes), so it works
under a strict `style-src 'self'` Content-Security-Policy, including nonce mode.
The default colors are defined with `--flash-*` CSS custom properties (with
hard-coded fallbacks), so you can re-theme them by setting the variables on
`:root`, or override the `.flash` / `.flash-<level>` classes in your own CSS.

## Levels

| Method | Level | Typical use |
| --- | --- | --- |
| `flash.success(msg)` | `success` | "Saved", "Created", "Welcome back" |
| `flash.info(msg)` | `info` | Neutral status, "You have been signed out" |
| `flash.warning(msg)` | `warning` | "Your trial ends soon" |
| `flash.error(msg)` | `error` | "Invalid credentials", validation failures |

The lower-level `flash.push(level, msg)`, `flash.peek()` (read without clearing),
and `flash.consume()` (read and clear, returning the raw `Vec<FlashMessage>`) are
available when you need to render messages yourself.

## htmx behavior

Flash works with htmx-driven flows in two ways:

- **Out-of-band swap.** For an htmx response that swaps a fragment (rather than
  navigating), include `flash.render_oob().await` anywhere in the response. It
  emits the same `<div id="flash">` container marked `hx-swap-oob="true"`, so htmx
  replaces the flash region already present in the page — notices appear without a
  full reload.

  ```rust
  #[post("/items/{id}/star")]
  async fn star(flash: Flash, /* ... */) -> AutumnResult<Markup> {
      flash.success("Starred.").await;
      Ok(html! {
          (flash.render_oob().await)   // updates #flash in place
          // ... the fragment being swapped ...
      })
  }
  ```

- **`HX-Trigger` header.** Alternatively, `flash.inject_hx_trigger(response).await`
  consumes the pending messages and emits an `HX-Trigger: {"flash": [...]}` header,
  letting client-side htmx listeners render them however you like.

For htmx redirects (`HX-Redirect` / `HX-Location`), the browser performs a full
navigation to the target page, where the ordinary `flash.render()` in your layout
shows the notice — no extra work needed.

## Notes

- Flash state lives in the session, so it inherits the same signed/HMAC cookie
  protection as everything else in the session. Changing the session backend
  (memory, Redis, …) requires no flash changes.
- `consume()` / `render()` are one-shot: a message is shown on the next request
  and then gone.
- Logging out is a special case — fully destroying the session would discard a
  pending flash. The generated `auth` logout clears the session data and rotates
  the id instead, which is equivalent for replay safety (the old cookie can no
  longer be used) while still carrying a "signed out" notice to the login page.
  The failed-login path deliberately keeps its hardened, non-enumerating response
  and does **not** write a session-backed flash, so an anonymous attacker cannot
  amplify session storage by hammering the endpoint.
