# SSRF via subscriber-chosen `target_url` in outbound webhook delivery (2026-09-03)

**Class:** SSRF reaching the internal network / cloud metadata, through a
documented, first-class feature
**Surface:** `autumn_web::webhook_outbound` — `deliver_webhook_job` (the
`autumn_webhook_delivery` background job)
**Entry point:** `WebhookOutboundManager::dispatch()` → `autumn_webhook_delivery`
job → `deliver_webhook_job`, the framework's own delivery of a
`WebhookSubscription::target_url`
**Affected:** `autumn-web` 0.7.0 and every earlier release that shipped
`webhook_outbound`
**Status:** fixed — `autumn/src/webhook_outbound.rs`, `autumn/src/http_client.rs`

## 🕵️ Threat model

> Against an app that follows Autumn's documented outbound-webhook guide
> (`docs/guide/outbound-webhooks.md`) — lets its own users/customers register a
> `WebhookSubscription` naming a `target_url` ("a consumer's registered
> endpoint", per the guide) and calls `WebhookOutboundManager::dispatch()` on
> domain events — an attacker who is an ordinary authenticated user of that
> app, exactly the principal the feature is built to serve, can set
> `target_url` to an address on the app's own network (an internal admin
> service, the database host, a message queue) or a cloud metadata endpoint
> (`http://169.254.169.254/latest/meta-data/iam/security-credentials/...`).
> Autumn's own background job then makes the outbound HTTP POST from inside
> the app's network/cloud environment, on the attacker's behalf, and (via the
> stored delivery log / DLQ, and the `/actuator/webhooks/dlq` actuator route
> the guide documents) the attacker can read back the response body — the
> credentials the metadata service returned, or the internal service's
> reply. The app author did nothing the documentation told them not to do:
> the guide's only stated security mechanism is the outbound HMAC-SHA256
> payload signature (§3), which says nothing about, and does nothing to
> constrain, where the request is allowed to go.

This is exactly the SSRF-via-webhook shape well known from Stripe/GitHub/
Shopify-style "notify my server" integrations — the reason those platforms all
validate or restrict the destination before dialing it.

## 🔎 Root cause

Autumn ships a real SSRF-safe outbound path: `Client::get_ssrf_safe`
(`autumn/src/http_client.rs`) resolves the destination host, rejects the
request if *any* resolved address is on the built-in
private/loopback/link-local/CGNAT/cloud-metadata deny-list
(`is_blocked_ip`), and pins the connection to the validated address set so a
DNS-rebinding race cannot swap in a blocked address after the check.

`deliver_webhook_job` — the one place in the framework that dials a
completely attacker/subscriber-chosen URL — did not use it:

```rust
let req = manager
    .client
    .named(&sub.target_url)
    .post(&sub.target_url)          // plain POST — no SSRF policy applied
    .header("Content-Type", "application/json")
    .header("Autumn-Signature", signature_header)
    .text_body(log.payload.clone());
```

`get_ssrf_safe` could not have been used as-is even if someone had reached for
it: it is hard-coded to `Method::GET`

```rust
pub fn get_ssrf_safe(&self, url: impl Into<String>) -> RequestBuilder {
    let mut builder = self.build_request(Method::GET, url.into());
    builder.ssrf_safe = true;
    builder
}
```

even though the `ssrf_safe` flag it sets, and the whole resolve→validate→pin
path it enables in `RequestBuilder::send`, are method-agnostic — `send_ssrf_safe`
reads `self.method` and works for any verb. The one outbound call that most
needed the guarantee (a POST to a value the app did not choose) was also the
one call the public API had no way to opt into it for.

## 🧪 Reproduction

`autumn/src/webhook_outbound.rs::tests::deliver_webhook_job_refuses_ssrf_target_url`

```bash
cargo test -p autumn-web --lib webhook_outbound::tests::deliver_webhook_job_refuses_ssrf_target_url
```

The test binds a real loopback `TcpListener` (standing in for an internal
service / cloud metadata endpoint — `127.0.0.1` is on the framework's own
deny-list) as `target_url`, runs `deliver_webhook_job` with no HTTP mock
registered (the mock path would short-circuit before the SSRF check ever
runs), and asserts the listener never receives a connection.

On trunk (`trunk-failure.txt`): the listener **does** receive the POST — the
assertion that it must not fails, proving the framework dialed the
SSRF-blocked destination.

## 🩹 Fix

Two changes, both additive:

1. **`autumn/src/http_client.rs`** — `RequestBuilder::ssrf_safe()`: a new
   public chainable builder method that sets the same `ssrf_safe` flag
   `get_ssrf_safe` sets, usable after `Client::post`/`put`/`patch`/`delete`,
   not just `get`. `Client::get_ssrf_safe` is refactored to call it
   (`self.build_request(Method::GET, url.into()).ssrf_safe()`) — byte-identical
   behavior, no signature change.
2. **`autumn/src/webhook_outbound.rs`** — `deliver_webhook_job`'s delivery
   request chains `.ssrf_safe()` before `.send()`.

Delivery to a blocked destination now fails closed with
`ClientError::SsrfBlocked` before any socket is opened, is recorded on the
delivery log exactly like any other transport failure (`last_error`, retried
up to `max_attempts`, DLQ'd on exhaustion), and reaches the same operator-only
`/actuator/webhooks/dlq` surface the guide already documents — nothing new is
exposed to the attacker beyond "delivery failed."

## ✅ Verification

* `cargo test -p autumn-web --lib webhook_outbound::tests::deliver_webhook_job_refuses_ssrf_target_url` — red before, green after (see `trunk-failure.txt` / `after.txt`).
* `cargo test -p autumn-web --lib webhook_outbound::` — full module, all existing tests still pass unchanged (mock-backed tests are unaffected: `send_recorded` checks `self.mock.is_some()` before the SSRF branch, so a registered mock still short-circuits exactly as before).
* `cargo test -p autumn-web --lib http_client::` — full module, all existing `get_ssrf_safe` tests still pass unchanged after the refactor.
* `cargo fmt --all`
* `cargo clippy --workspace --all-targets -- -D warnings`
* `./scripts/pre-push-check.sh`

## 📡 Blast radius

Swept every other framework call site that dials a URL the app did not
choose, grepping for `.post(`/`.put(`/`.patch(`/`.delete(` against a value
sourced from stored/request data rather than a literal or a `[http.client]`
config alias:

* **`webhook_outbound.rs::deliver_webhook_job`** — fixed above (the finding).
* **OAuth2 / OIDC token & userinfo endpoints** (`oauth2.rs` and friends) —
  checked: the authorization/token/userinfo/JWKS URLs come from the app's own
  `[oauth.providers.*]` config, not from request/subscriber data, so they are
  not attacker-controlled the way `target_url` is; out of scope for this
  finding (a compromised config is a different threat model).
* **`http_client.rs` `follow_redirects`/`pin_to`/plain `get`/`post`/etc.
  call sites inside `autumn/src`** — none dial a subscriber/request-supplied
  URL directly; every other in-framework caller either targets a
  `[http.client.base_urls]` alias or an app-configured upstream.
* **MCP outbound tool calls** — `mcp.rs` dispatches back into the app's own
  router (loopback, in-process), not an outbound `Client` call; not this
  shape.
* Feature-gated call sites: none of `redis`, `ws`, `mail`, `i18n` touch
  `http_client`'s outbound path.

No sibling gap found; this was the one call site.

## 📜 Compatibility

Both changes are pure additions — `RequestBuilder::ssrf_safe()` is new,
`Client::get_ssrf_safe`'s signature and behavior are unchanged, and
`deliver_webhook_job`'s request now fails a class of destination
(private/loopback/link-local/CGNAT/cloud-metadata) it previously reached.
That is the security fix, and per CLAUDE.md and the "Ask before" gate below,
worth flagging explicitly: **an app that was relying on `target_url` pointing
at a private address (e.g. an internal-only receiver during development)
will see those deliveries start failing with `SsrfBlocked` after upgrade.**
That is the correct behavior for the documented, subscriber-facing feature,
matching what `get_ssrf_safe` already enforces elsewhere in the framework, but
it is a behavior change for any app relying on the previous unrestricted
delivery. Recorded under `## [Unreleased] → Security` in `CHANGELOG.md`.
