# Web Push

A notification only re-engages a user if it reaches them when they are *not*
looking at your app. The [in-app feed](notifications.md) is durable but only
visible once they come back, and `channels` needs a live connection — both go
quiet the moment the tab closes.

Web Push is the leg that closes that gap: the browser holds a subscription with
its push service (FCM, Mozilla autopush, WNS), and your app hands that service
an encrypted payload to deliver whenever the device next comes online. Autumn
ships the whole loop — key handling, subscription storage, the browser-facing
routes, the service-worker handlers, and the cryptography — so **you write zero
lines of crypto**.

> **Scope.** This is browser Web Push (VAPID / Push API). Native mobile push
> (APNs/FCM device tokens) is out of scope, as are notification actions,
> images, badge counts, and per-user quiet hours.

## Quick start

### 1. Mint a VAPID key pair

A VAPID key pair identifies your application server to push services. Mint one
**once**, offline, and keep the private half secret:

```rust
let key = autumn_web::push::VapidKey::generate();
println!("private_key = \"{}\"", key.private_key_base64url());
println!("public_key  = \"{}\"", key.public_key_base64url());
```

### 2. Configure it

```toml
[push]
private_key = "…"                     # from step 1 — secret
public_key  = "…"                     # optional; see "Fail fast" below
subject     = "mailto:ops@example.com"
```

Supply `private_key` from an environment variable
(`AUTUMN_PUSH__PRIVATE_KEY`) or the [encrypted credentials
store](credentials.md) rather than committing it. Every field has an
environment override: `AUTUMN_PUSH__PUBLIC_KEY`, `AUTUMN_PUSH__SUBJECT`,
`AUTUMN_PUSH__TTL_SECS`.

`subject` must be a `mailto:` or `https:` URI — RFC 8292 requires it, and it is
how a push service operator reaches you about your traffic. A bare email
address is the common mistake and is refused at boot, because otherwise the app
starts fine and every delivery is rejected remotely.

### 3. Generate the PWA

```bash
autumn generate pwa
autumn migrate
```

That emits (or updates):

- `static/service-worker.js` — with `push` and `notificationclick` handlers.
- `static/pwa-register.js` — the client subscribe snippet.
- `migrations/<ts>_create_push_subscriptions/` — the subscription table.
- `src/main.rs` — mounts `autumn_web::push::router()`.

`--dry-run` is honored, and `autumn destroy pwa` reverses every part of it. A
re-run needs `--force` (as every Autumn generator does — a plain re-run refuses
rather than overwriting files you may have edited); with it, the result is
idempotent: the migration directory is reused rather than duplicated, and the
handlers, tags, and router mount are not added twice.

If your `src/main.rs` no longer has a recognizable builder chain, the generator
says so and skips the mount rather than guessing. Add it yourself:

```rust
.merge(autumn_web::push::router())
```

### 4. Let a user opt in

The generated snippet exposes a declarative opt-in, so a button is all the
markup you need:

```html
<button data-autumn-push-subscribe>Enable notifications</button>
```

It is deliberately **not** wired to page load: a permission prompt fired
without a user gesture is the fastest way to get permanently blocked, and both
Chrome and Firefox penalise it. Call `window.autumnPushSubscribe()` yourself if
you want your own trigger; `window.autumnPushUnsubscribe()` is the inverse.

### 5. Send

```rust
use autumn_web::prelude::*;

#[post("/builds/{id}/fail")]
async fn build_failed(push: WebPush, id: Path<i64>) -> AutumnResult<&'static str> {
    push.send(
        owner_id,
        &PushMessage::new("Build failed", "main is red")
            .url(format!("/builds/{}", *id)),
    )
    .await?;
    Ok("ok")
}
```

That is the whole glue budget: configure the key, mount the router, call
`send`. Everything below the line — the ES256 VAPID JWT (RFC 8292), the ECDH
key agreement, HKDF derivation, and AES-128-GCM payload encryption (RFC 8291) —
is the framework's.

## The API

`WebPush` is an extractor, surfaced like `Session`, `Db`, and `Notifications`.

| Method | Behavior |
|---|---|
| `send(principal, &message)` | Deliver to every device that principal has subscribed |
| `send_many(principals, &message)` | Fan out across principals, aggregating one report |
| `subscribe(principal, &browser_subscription)` | Validate and record a browser subscription |
| `unsubscribe(principal, endpoint)` | Remove one of that principal's subscriptions |
| `vapid_public_key()` | The `applicationServerKey` the browser subscribes with |

`PushMessage::new(title, body)` optionally takes `.url(…)` (where a click
navigates) and `.icon(…)`.

### Principals

`send` takes anything convertible into a `PushPrincipal`, which covers both id
shapes Autumn apps use:

```rust
push.send(user.id, &message).await?;        // i64, as the notification feed uses
push.send("service:ci", &message).await?;   // a string principal, as auth tokens carry
```

`PushPrincipal::from(42_i64)` and `PushPrincipal::from("42")` are the same
principal, so composing with the in-app feed needs no conversion.

### The delivery report

`send` returns a `PushDeliveryReport`, because one unreachable device must not
fail the whole call — a user with three devices, one of which has revoked
permission, still gets the notification on the other two:

```rust
let report = push.send(user.id, &message).await?;
report.delivered;  // accepted by the push service
report.pruned;     // endpoints reported gone (404/410) and now removed
report.failed;     // failed for a transient reason; still in the store
```

A principal who never subscribed yields an empty report, not an error.

## Built-in routes

`autumn_web::push::router()` mounts three routes:

| Method | Path | Auth |
|---|---|---|
| `GET` | `/push/vapid-public-key` | public — it is public key material |
| `POST` | `/push/subscribe` | signed in |
| `POST` | `/push/unsubscribe` | signed in |

The mutating routes resolve the caller **server-side** — from the framework's
current actor (set by session and bearer-token auth alike), falling back to the
`[auth] session_key` session value. They never take a principal from the
request body, which would let anyone subscribe *as* anyone. Until an app has
authentication these routes are simply dormant: every call 401s and nothing is
stored.

Unsubscribe is scoped to the caller, so one signed-in user cannot drop another
user's device.

### The principal must be the same id you send to

> **This is the one thing to get right.** The router binds a subscription to
> whatever the request's authenticated principal is; `send` looks a principal
> up by the id *you* pass. If those two disagree — the router records
> `"user:42"` because a bearer token published that, while you call
> `push.send(42_i64, …)` — the send finds nothing and returns an empty report:
> zero delivered, **no error**.

With ordinary session auth the session value *is* the user id, so
`push.send(user.id, …)` matches and there is nothing to do. If your app
authenticates with API tokens whose `principal_id` is namespaced, either send
with the same string (`push.send("user:42", …)`) or mount your own subscribe
handler that calls `push.subscribe(user.id, …)` with the id you send to. A send
that finds no subscriptions logs at `debug` with the principal it looked up,
which is the quickest way to spot a mismatch.

Both `PushPrincipal::from(42_i64)` and `PushPrincipal::from("42")` produce the
same principal, so the `i64` and string spellings of one id are
interchangeable — but two *different* namespaces are not.

## Composing with the in-app feed (#1148)

Push is a *delivery leg*, not a replacement for the feed. The feed is the
durable record the user can come back to; the push is the nudge that brings
them back. Write the record first and await it, then push best-effort:

```rust
use autumn_web::prelude::*;
use autumn_web::push::PushMessage;

#[post("/posts/{id}/comments")]
async fn comment(
    notifications: Notifications,
    push: WebPush,
    id: Path<i64>,
) -> AutumnResult<&'static str> {
    // The durable record. A failure here IS a failure of the request.
    notifications
        .notify(author_id, "comment.created", serde_json::json!({ "post": *id }))
        .await?;

    // The nudge. A push service outage, a revoked permission, or an app with
    // no key configured must never fail the comment that was already written.
    if let Err(e) = push
        .send(
            author_id,
            &PushMessage::new("New comment", "Someone replied to your post")
                .url(format!("/posts/{}", *id)),
        )
        .await
    {
        tracing::warn!(error = %e, "web push failed; the in-app notification still stands");
    }

    Ok("ok")
}
```

This mirrors how `Notifications::notify_with_push` treats its `channels`
broadcast: the durable write is propagated, the delivery attempt is logged.

## Storage

A subscription is an endpoint URL plus two keys, bound to a principal. The
store resolves exactly like the notification feed's:

1. A store registered via `AppBuilder::with_push_subscription_store(...)`.
2. `DbPushSubscriptionStore` when a database pool is configured — the
   `push_subscriptions` table `autumn generate pwa` scaffolds.
3. `MemoryPushSubscriptionStore` otherwise (process-local; what `TestApp`
   without a database uses).

`endpoint` is the primary identity and carries a UNIQUE constraint, which is
what makes the upsert atomic. Two consequences worth knowing:

- Re-subscribing the same browser **updates** the row, and the endpoint is
  normalized first, so `https://x/p` and `https://x:443/p` can never become two
  rows for one device.
- Re-subscribing under a *different* principal **moves** the row, but only when
  the request presents **both** stored keys — `p256dh` *and* `auth`. That is exactly
  the shared-device case — a second user signs in, the browser returns the same
  endpoint *and* the same keys — while refusing the attack it would otherwise
  allow: an endpoint URL is only a capability to *send*, so anyone who obtained
  one could otherwise re-register it under their own account with their own
  keys, cutting the victim off and redirecting their notifications. A refused
  move is `PushError::EndpointClaimed` (`409`).
- One principal may hold at most `MAX_SUBSCRIPTIONS_PER_PRINCIPAL` (20)
  subscriptions. Past that, `subscribe` is
  `PushError::TooManySubscriptions` (`422`). Without a ceiling, one account
  could make every send unbounded work.

## Fail fast, never silently

The failure this subsystem is built to prevent is an app that starts cleanly,
happily records subscriptions, and silently never delivers anything.

- A `[push] private_key` that is present but unusable — a typo, an env var that
  failed to interpolate, an empty string — **fails the boot** with a named
  error. An app with no `[push]` block at all is unaffected.
- A **blank** `AUTUMN_PUSH__PRIVATE_KEY` is treated as "this secret failed to
  interpolate", not as "push disabled". Most `AUTUMN_*` overrides clear their
  setting when set to an empty string; this one deliberately does not, because
  clearing it would silently disable delivery — and would erase a good key from
  `autumn.toml` besides.
- `subject` is validated wherever it comes from. The `[push]` block is checked
  at boot; a service built by hand and registered with
  `AppBuilder::with_web_push` is checked on the first `send`, before anything
  is dispatched, since it never passes through boot validation.
- Declaring `public_key` is optional and exists purely as a safety check: a
  mismatched pair is caught at boot rather than surfacing as every send being
  rejected by the push service with no local symptom.
- Calling `send` with no key configured is a `PushError::NotConfigured`, raised
  before anything is dispatched. It is never an `Ok` report of zero deliveries.
- `GET /push/vapid-public-key` answers `503` when push is unconfigured, so the
  client can tell that apart from "here is your key".

### Pruning

RFC 8030 gives push services `404 Not Found` and `410 Gone` to say a
subscription no longer exists. Autumn removes those rows and reports them in
`report.pruned`, so a dead endpoint is never re-sent to.

Every *other* failure — a `5xx` outage, a `429` rate limit, a transport error —
is counted in `report.failed` and the subscription is **left in place**.
Pruning on a transient failure would silently unsubscribe every user during an
incident, unrecoverable without each of them re-granting permission.

## Endpoint safety

The endpoint is a URL supplied by the client that the framework later `POST`s
to, which makes an unvalidated subscribe route a server-side request forgery
(SSRF) gadget. Two rules close it at the boundary, before anything is stored:

- The scheme must be `https`, so neither the encrypted body nor the VAPID JWT
  that authenticates your server crosses a plaintext hop.
- The host must be a domain name that is not `localhost`. Every real push
  service publishes a hostname, so refusing IP literals outright is stricter
  and simpler than enumerating private ranges — `https://169.254.169.254/…`,
  `https://10.0.0.1/…` and `https://[::1]/…` all fail the same rule.

A hostname that *resolves* to a private address is caught at dispatch time by
the outbound client's own SSRF address policy.

## CSRF

Autumn's CSRF layer rejects an unaccompanied `POST`, and its cookie is
`HttpOnly` — so the subscribe snippet cannot read a token the usual way. The
public-key response carries one for it:

| Response header | Carries |
|---|---|
| `x-autumn-push-csrf-token` | the caller's CSRF token |
| `x-autumn-push-csrf-header` | the header name to send it back in |

The generated snippet fetches that endpoint before subscribing anyway, so this
costs no extra request. Reading the headers requires a **same-origin** fetch —
a cross-origin attacker cannot see response headers without the app opting in
via CORS, which is the same property double-submit CSRF already relies on, and
same-origin script could take a token from any rendered form regardless.

If your app already publishes `<meta name="csrf-token">` (as
`autumn generate auth` does), the snippet uses that as a fallback.

**Do not exempt `/push/*` from CSRF.** It looks like the easy fix and it is the
worst one available: a forced subscribe registers the *attacker's* keys under
the victim's session, letting them decrypt every notification you send that
user.

## Testing

`RecordingPushTransport` records requests instead of sending them, so you can
assert exactly what would have gone to the push service — and drive the stale
subscription path without waiting for a real expiry:

```rust
use autumn_web::push::{
    MemoryPushSubscriptionStore, PushMessage, RecordingPushTransport, VapidKey, WebPush,
};

#[tokio::test]
async fn build_failure_pushes_the_owner() {
    let transport = RecordingPushTransport::new()
        // Optional: make one endpoint report itself gone.
        .responding_with("https://push.example.com/dead", 410);

    let push = WebPush::new(
        MemoryPushSubscriptionStore::new(),
        VapidKey::generate(),
        "mailto:ops@example.com",
        transport.clone(),
    );

    let client = TestApp::new()
        .merge(autumn_web::push::router())
        .with_web_push(push.clone())
        .build();
    client.acting_as(7).await;

    // … subscribe over HTTP, then send …

    let sent = transport.requests();
    assert_eq!(sent[0].endpoint, "https://push.example.com/live");
    assert!(sent[0].header("authorization").unwrap().starts_with("vapid t="));
}
```

## Signing out

A subscription that outlives the session that created it is a privacy leak: the
row stays bound to the user who signed out, so the app keeps pushing their
notifications to that browser — in front of whoever uses the device next.

The generated snippet handles it. It intercepts any form posting to `/logout`
(what `autumn generate auth` emits) and unsubscribes first, while the session
is still valid enough for the server to authorize it. If your sign-out is
shaped differently, mark it:

```html
<form action="/session/end" method="post" data-autumn-push-unsubscribe>
```

A failed unsubscribe never blocks the sign-out itself.

## Payload limits

Push services are only required to accept a 4096-byte encrypted body, which
leaves 3993 bytes for the JSON payload. Exceeding it is a
`PushError::PayloadTooLarge` raised **before** any dispatch, rather than N
identical rejections from the push service. Keep the notification short and put
the detail behind `.url(…)`.

## What's under the hood

You do not need any of this to use the feature, but for the record:

| Concern | Standard | Where |
|---|---|---|
| Application server identity | RFC 8292 (VAPID) | ES256 JWT, `Authorization: vapid t=…, k=…` |
| Payload encryption | RFC 8291 | ECDH P-256 + HKDF-SHA256 + AES-128-GCM |
| Body framing | RFC 8188 | `aes128gcm` content coding |
| Protocol | RFC 8030 | `TTL` header, `404`/`410` semantics |

The encryption is pinned to RFC 8291 §5's own published test vector: fed the
RFC's inputs, Autumn must reproduce the RFC's output byte-for-byte. That is
what guarantees the payload actually decrypts in a real browser rather than
merely round-tripping against Autumn's own code.

Adding Web Push introduced **no new crate** into the dependency graph: `p256`
was already resolved for `jsonwebtoken`'s ES256 backend, and
`aes-gcm`/`hmac`/`sha2`/`base64` were already non-optional dependencies.
