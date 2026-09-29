# One-Time Submit Tokens

A one-time submit token gives a plain HTML form an **at-most-once** guarantee:
the mutating request behind it runs exactly once, no matter how many times the
browser sends it. A user who double-clicks **Submit**, hits Back and resubmits,
or whose browser silently retries a flaky POST cannot create a duplicate record.
It is server-side, default-on, and needs **no client-side JavaScript**.

The whole feature is one hidden form field plus a Tower layer that the framework
applies for you.

---

## The problem

The classic double-submit hole is not closed by the two primitives that sit next
to it:

- **CSRF tokens** mint a *stable, per-session* value. A valid `_csrf` token
  submitted twice passes both times — CSRF stops a forged request, not a
  duplicate one.
- **Idempotency keys** ([`idempotency.md`](idempotency.md)) are *header-driven*
  on `Idempotency-Key`. API clients send that header; a browser submitting a
  bare `<form>` never does.

So a create form with only CSRF protection still lets a double-click insert two
rows. A `UNIQUE` constraint may catch some of those, but many mutations (post a
comment, charge a card, send an invite) have no natural uniqueness key.

Submit tokens close the gap: a fresh, single-use token is minted per render,
embedded in the form, and consumed against a shared idempotency store on the
POST. The first use runs the handler and records its response; a replayed token
short-circuits and replays that first response instead of re-running anything.

---

## Quick start

Submit-token protection is **on by default** — the framework installs
[`SubmitTokenLayer`] whenever `security.submit_token.enabled = true` (the
default). You do not construct the layer yourself. Two steps opt an individual
form in:

1. Take the [`SubmitToken`] extractor in the handler that renders the form and
   emit its value in a hidden `_submit_token` field.
2. Do nothing else — the layer consumes that field on the matching POST.

```rust,ignore
use autumn_web::prelude::*;
use autumn_web::security::SubmitToken;

#[get("/form")]
async fn form(submit_token: SubmitToken) -> Markup {
    html! {
        form method="POST" action="/submit" {
            input type="hidden" name="_submit_token" value=(submit_token.token());
            input type="text" name="title";
            button { "Submit" }
        }
    }
}

#[post("/submit")]
async fn submit(Form(form): Form<CreateForm>) -> AutumnResult<Redirect> {
    // Runs exactly once per rendered token. A double-click replays the redirect
    // below instead of inserting a second row.
    create_record(form).await?;
    Ok(Redirect::to("/thanks"))
}
```

The POST handler needs no submit-token parameter and no special return type: the
layer guards the request *before* it reaches the handler, so the handler stays a
plain create action. Because serde ignores unknown fields by default, the extra
`_submit_token` value on the form body is simply not part of your `CreateForm`
struct.

---

## How it works

`SubmitTokenLayer` wraps every request:

1. **Mint.** On *every* request (the GET that renders the form and the re-render
   after a rejected POST alike) the layer generates a fresh random token and
   places it in request extensions. The [`SubmitToken`] extractor reads it back.
2. **Scan.** On a mutating request (`POST`/`PUT`/`PATCH`/`DELETE`) the layer
   scans the form body for the configured field (`_submit_token`). A request
   with no such field passes straight through unchanged, so only forms that
   embed the field are guarded.
3. **Consume.** The token is looked up in the shared idempotency store:
   - **First use** — the layer takes an in-flight lock, runs the handler, and
     records the response (2xx/3xx) under the token before releasing the lock.
   - **Replayed token** — the stored first response is returned verbatim, tagged
     with an `x-submit-token-replayed` header. The handler never runs again.
   - **Concurrent duplicate** — a second request racing the first loses the lock
     and gets a `409 Conflict` rather than re-running the mutation.

Because the token is a per-render random UUID it is globally unique and needs no
method, path, or principal scoping — the token *is* the identity of the single
logical submission.

### Where the state lives

Consumed tokens are stored in the same backend as
[idempotency keys](idempotency.md). By default the submit-token store **inherits
`[idempotency].backend`**: in-memory in development, Redis in production. That
inheritance matters for horizontal scaling — with a shared Redis store a
double-click that load-balances to a *different* replica still sees the token as
consumed and replays the first response, so the mutation cannot run twice across
your fleet.

---

## Configuration

All keys live under `[security.submit_token]` and have working defaults:

```toml
[security.submit_token]
enabled = true          # default; installs the layer
field_name = "_submit_token"
ttl_secs = 600          # replay window for a consumed token's response (10 min)
in_flight_ttl_secs = 86400  # safety expiry for an in-flight lock (24 h)
# backend inherits [idempotency].backend when unset
# backend = "redis"     # override for submit tokens only
exempt_paths = []       # path prefixes skipped by the guard
```

| Key | Default | Meaning |
|-----|---------|---------|
| `enabled` | `true` | Install the layer at all. |
| `field_name` | `"_submit_token"` | Hidden field the guard reads. |
| `ttl_secs` | `600` | How long a consumed token replays its first response. |
| `in_flight_ttl_secs` | `86400` | Safety expiry for the in-flight lock; independent of `ttl_secs` so lowering the replay window can never shorten how long an active submission is excluded from re-entry. |
| `backend` | inherits `[idempotency].backend` | Store for consumed tokens. |
| `exempt_paths` | `[]` | Path prefixes the guard skips. |

If you customise `field_name`, read the resolved name from the
[`SubmitFormField`] extractor rather than hard-coding it, so the hidden input's
`name` always matches what the guard scans for.

> **Production note.** An explicit `[security.submit_token].backend = "memory"`
> in production fails startup fast — an in-memory store is per-process and cannot
> protect a multi-replica deployment. Leave `backend` unset (to inherit Redis) or
> set it explicitly to `"redis"`.

---

## Related

- [Idempotency keys](idempotency.md) — the header-driven sibling for API
  clients, and the store submit tokens reuse.
- Nested `has_many` forms (`nested_form`) already integrate submit tokens via
  `NestedChangesetForm::with_submit_token`, so a master-detail form inherits the
  same at-most-once guarantee.

See the **saas** example (`examples/saas/src/routes/auth.rs`) for a real
signup form guarded by a one-time submit token.
