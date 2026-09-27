# CORS and Cross-Origin Requests

If a browser on `https://app.example.com` fetches your Autumn app on
`https://api.example.com`, the browser — not Autumn — decides whether the
JavaScript is allowed to read the response. It decides by looking for
`Access-Control-*` headers, and Autumn sends those only when two things hold: the
request's `Origin` is one you listed in `[cors]`, **and** the response passes
through the CORS middleware. Each half has its own failure mode, and both are
below.

This page is the `[cors]` section: what each key does, which default a given build
actually gets, and the interactions — CSRF above all — that leave a correct origin
list still not enough. It assumes you already have a route that works when you
`curl` it, and the problem is a browser on another origin.

If you are calling your app from its own origin — a server-rendered page, an
htmx fragment, a form post — you do not need CORS at all, and enabling it
changes nothing.

## Quick start: enable CORS

**Which default you get depends on the profile**, and the difference is the thing
this page exists to warn about. Autumn's base default is an empty origin list, so
CORS is off — but the `dev` profile seeds `allowed_origins = ["*"]`, so under
`dev` it is on and permissive.

Which profile you get with no `AUTUMN_ENV`, `AUTUMN_PROFILE` or `--profile` set
follows the build: `#[autumn_web::main]` reports the build mode, and a release
build resolves to `prod` (list empty, CORS off) while a debug build falls through
to `dev` (`["*"]`, CORS open). So the usual pairing is a permissive dev machine
and a closed production deploy — but a release binary run locally is closed too,
and `AUTUMN_ENV` overrides all of it.

Don't infer which you got — ask the app. `AUTUMN_SHOW_CONFIG=1` (or
`autumn dev --show-config`) logs the resolved configuration at startup, including
the active profile and the middleware actually installed. That report is off
unless you ask for it.

To list them, in `autumn.toml`:

```toml
[cors]
allowed_origins = ["https://app.example.com"]
```

Or via an environment variable, for a deployment override without editing
config files:

```
AUTUMN_CORS__ALLOWED_ORIGINS=https://app.example.com,https://admin.example.com
```

That covers **reads**. The default methods already include the mutating verbs —
`GET`, `POST`, `PUT`, `DELETE`, `PATCH`, `OPTIONS` — and the default
`allowed_headers` are `Content-Type` and `Authorization`.

**Mutating requests also meet CSRF under `prod`.** The `prod` profile turns CSRF
on, so a cross-origin `POST`/`PUT`/`PATCH`/`DELETE` needs a valid token as well as
an allowed origin. Whether `[cors]` needs anything depends on how the token
travels: a client that sends it in a header needs that header
(`security.csrf.token_header`, default `X-CSRF-Token`) added to
`allowed_headers`, or the browser will not send it and the preflight fails before
the request is attempted —

```toml
[cors]
allowed_origins = ["https://app.example.com"]
allowed_headers = ["Content-Type", "Authorization", "X-CSRF-Token"]
```

— while a form-encoded submission can carry the token in the form field instead
and needs no change here. Listing the header permits the token; it does not exempt
the request. Safe methods (`GET`, `HEAD`, `OPTIONS`, `TRACE`) never meet any of
this, which is why a read-only integration does not.

[Forms, Validation and Normalization](forms.md) owns the token mechanics,
including `security.csrf.exempt_paths` for an API authenticated by a bearer token
rather than a cookie.

## Allowed origins, and the empty default

`allowed_origins` is the switch. When it is **empty — the base default, and what
`prod` leaves in place — the CORS middleware is not installed at all**: no
`Access-Control-*` header is sent on any response, and a cross-origin browser
call fails no matter what the other four keys say.

Two profile smart defaults act on this key, and they differ:

| Profile | `allowed_origins` default | Effect |
|---------|---------------------------|--------|
| `dev` | `["*"]` | any origin may call you, so a local front-end on another port works out of the box |
| everything else, `prod` included | `[]` | CORS disabled until you list origins explicitly |

This is the step that surprises people at deploy time: a front-end that worked
all the way through development stops being able to read responses in
production, because `dev` was permissive and `prod` is not. Nothing is broken —
the origin list is simply empty, and you have to supply it.

Origins are matched exactly, scheme and port included. `https://example.com`
does not cover `https://www.example.com`, `http://example.com`, or
`https://example.com:8443`; list each one you actually serve.

## CORS with cookies and credentials

**First, the case that does not need this section.** If your cross-origin client
authenticates by setting an `Authorization: Bearer …` header itself, that is an
ordinary request header, not a credential in the CORS sense: it needs only to be
in `allowed_headers` — where `Authorization` already is by default — and
`allow_credentials` has nothing to do with it. Leave it `false`. Turning it on for
a bearer-token client gains nothing and costs the wildcard origin, which the
validation below rejects in combination with credentials.

"Credentials" here means the things a browser attaches *ambiently* — cookies
above all. `allow_credentials = true` makes Autumn send
`Access-Control-Allow-Credentials: true`, the server's half of letting a
cross-origin caller use them:

```toml
[cors]
allowed_origins = ["https://app.example.com"]
allow_credentials = true
```

**That header is necessary and not sufficient**, and the rest is not Autumn's to
decide. The caller must request credentials (`credentials: "include"` on a
`fetch`), and the browser must be willing to send the cookie at all, which turns
on its `SameSite` attribute and on whether your two origins count as same-site —
browser rules, which change on browser timelines. MDN's
[SameSite cookies][samesite] is the reference to check your exact pair against;
[Authentication](authentication.md) documents Autumn's session cookie, whose
`session.same_site` defaults to `"Lax"`.

**One limit is Autumn's own, and worth knowing before you design around it.** The
built-in CSRF protection issues its `autumn-csrf` cookie with `SameSite=Lax` fixed
in code — `security.csrf` has no setting for it. A genuinely cross-site browser
client therefore never receives that cookie, and a mutating request is rejected
with `403` however correctly it sends the token. So cross-site **and**
cookie-authenticated **and** mutating is not a combination the built-in stack
supports, and no `[cors]` or `[session]` value makes it one. The two shapes that
do work: a same-site deployment (subdomains of one domain, one scheme), or an API
authenticated by a bearer token with its paths in `security.csrf.exempt_paths`.

[samesite]: https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Set-Cookie#samesitesamesite-value

**`allow_credentials = true` and `allowed_origins = ["*"]` cannot be combined.**
Browsers reject that pair outright per the Fetch standard, so Autumn rejects it
at config load rather than letting you deploy it:

```
CORS: allow_credentials=true is incompatible with allowed_origins=["*"];
list explicit origins instead (browsers reject the wildcard+credentials combo)
```

Because `dev` seeds `allowed_origins = ["*"]`, adding `allow_credentials = true`
without also naming explicit origins is enough to hit this on your own machine.
List the origins.

## The `Access-Control-Allow-Origin` header, and what else Autumn sends

With an allowlisted origin calling you, a normal response carries:

| Response header | Comes from |
|-----------------|------------|
| `Access-Control-Allow-Origin` | the matched entry in `allowed_origins` |
| `Access-Control-Allow-Credentials` | sent as `true` only when `allow_credentials = true` |
| `Access-Control-Allow-Methods` | `allowed_methods` (preflight responses) |
| `Access-Control-Allow-Headers` | `allowed_headers` (preflight responses) |
| `Access-Control-Max-Age` | `max_age_secs` (preflight responses) |

These come from the CORS middleware, so they reach responses that pass through it.
One deployment does not: in a static-generation build, a cached `#[static_get]`
hit is served straight from the manifest and never reaches the CORS layer, so a
prerendered route fetched cross-origin still fails even with its origin
allowlisted. Serving those responses cross-origin needs CORS from an outer custom
layer, a reverse proxy, or the CDN in front.

A missing `Access-Control-Allow-Origin` does **not** tell you which of two
different problems you have, because both look identical in devtools: the layer
may not be installed (`allowed_origins` is empty), or it may be installed and
the request's `Origin` may simply not match any entry — an exact match on
scheme, host and port, so a differing port or `http` vs `https` misses.

To tell them apart, compare the two strings yourself rather than looking for a
signal from the framework. Devtools shows the exact `Origin` header the browser
sent on the request; put it next to the `allowed_origins` your app resolved, and
the answer is whichever of the two you find: an empty list, or a list that does
not contain that exact string.

Two startup signals help, each with a limit worth knowing before you rely on it:

- `AUTUMN_SHOW_CONFIG=1` (or `autumn dev --show-config`) logs a startup report
  listing the middleware actually installed, so you can see whether `CORS` is in
  the stack. It does **not** print the `[cors]` values, so it answers "is the
  layer there" and not "is my origin allowed".
- A `CORS enabled` line, logged when the layer is installed, **does** carry the
  origin list and the credentials flag — the most direct answer available. Treat
  its presence as evidence and its **absence as inconclusive**: it is emitted at
  `INFO`, as is the report above, so any `log.level` above `INFO` suppresses both
  on a perfectly working configuration.

A malformed entry is a third way to miss: an origin that will not parse as a
header value is dropped with a `CORS: ignoring malformed allowed_origin`
warning, and the rest of the list still applies — so a typo'd entry fails
without failing the boot.

## CORS preflight requests

Before a cross-origin request that is not a
[simple request][simple] — anything with a `Content-Type: application/json`
body, a custom header, or a `PUT`/`DELETE`/`PATCH` method — the browser sends an
`OPTIONS` request to the same URL and waits for permission. Autumn answers it
from your config; you do not write an `OPTIONS` handler, and the request never
reaches your route.

A preflight fails when the method is missing from `allowed_methods` or the
header is missing from `allowed_headers`. The browser reports this as a CORS
error on the *original* request, which is why a `PATCH` that works under `curl`
can fail from a page: `PATCH` is in the default `allowed_methods`, but a custom
header like `X-Request-Id` is not in the default `allowed_headers` and has to be
added.

`max_age_secs` (default `86400`, 24 hours) is how long the browser may cache a
successful preflight, so it is not re-sent before every call. That cache is why an
allowlist fix can appear not to work: the browser is still answering from the
preflight it cached under the old policy, and **lowering `max_age_secs` does not
evict an entry it already holds** — the new value applies only to the next
preflight it actually sends. To retest a change now, clear the browser's CORS
cache or use a fresh profile or private window. Lowering the value is worth doing
*before* a debugging session, not during one.

[simple]: https://developer.mozilla.org/en-US/docs/Web/HTTP/Guides/CORS#simple_requests

## Where CORS sits in the middleware stack

CORS is a config-gated layer, and it is the innermost of them — it sits inside
CSRF, and inside the request timeout. Two consequences worth knowing:

- A **CSRF rejection** on a cross-origin form post is produced *outside* the
  CORS layer, so it never flows back through it and carries no
  `Access-Control-*` headers. The browser reports that as a CORS failure, which
  hides the real `403`: if an allowlisted origin's POST fails while its GET
  succeeds, suspect CSRF before the origin list. See [Forms, Validation and
  Normalization](forms.md) — a cross-origin state-changing request needs a token
  as well as an allowed origin.
- A **synthesized 503** from the request timeout deliberately mirrors the CORS
  response headers, so a browser client can read the timeout instead of seeing
  it masked as a CORS failure.

[Middleware](middleware.md) has the full stack order, including the layers this
page does not touch.

## CORS and the `/mcp` endpoint

The `/mcp` JSON-RPC endpoint reuses `cors.allowed_origins` for a second purpose:
DNS-rebinding protection. It accepts a request when any of these holds, and
returns `403` before any parsing otherwise:

- it carries **no `Origin` header** at all — curl, SDKs and server-side agents
  are not subject to DNS rebinding, so an agent client needs no CORS
  configuration;
- its `Origin` is **the same origin as the request's own host**, and that host is
  a trusted host. A browser MCP client served by the app itself is therefore
  already allowed, without an `allowed_origins` entry;
- its `Origin` is listed in `cors.allowed_origins` (or the list holds `"*"`).

So it is specifically a **cross-origin** browser MCP client that needs its origin
added here. [Model Context Protocol](mcp.md) has the details, including how the
host is resolved behind a TLS-terminating proxy.

## S3 presigned uploads need a bucket CORS policy too

`[cors]` governs requests to **your app**. A browser `PUT` to a presigned S3 URL
does not touch your app, so it is governed by the **bucket's** CORS policy,
which you set in AWS. Configuring `[cors]` does not cover it, and neither
setting substitutes for the other — see
[File Uploads and Storage](storage.md).

## CORS configuration reference

```toml
[cors]
# Origins allowed to make cross-origin requests. Empty (the default) means the
# CORS middleware is not installed and no Access-Control-* headers are sent.
# The `dev` profile seeds ["*"]; `prod` leaves this empty.
allowed_origins = []

# Methods allowed on cross-origin requests.
allowed_methods = ["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"]

# Request headers a cross-origin caller may send.
allowed_headers = ["Content-Type", "Authorization"]

# Send Access-Control-Allow-Credentials: true, so the browser may send cookies.
# Incompatible with allowed_origins = ["*"]; rejected at config load.
allow_credentials = false

# How long a browser may cache a successful preflight, in seconds.
max_age_secs = 86400
```

| Environment variable | Type | Default |
|----------------------|------|---------|
| `AUTUMN_CORS__ALLOWED_ORIGINS` | comma-separated `String` | `[]` |
| `AUTUMN_CORS__ALLOWED_METHODS` | comma-separated `String` | `["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"]` |
| `AUTUMN_CORS__ALLOWED_HEADERS` | comma-separated `String` | `["Content-Type", "Authorization"]` |
| `AUTUMN_CORS__ALLOW_CREDENTIALS` | `bool` | `false` |
| `AUTUMN_CORS__MAX_AGE_SECS` | `u64` | `86400` |

[What Happens When…](what-happens-when.md#what-happens-when-cors-is-misconfigured)
walks the misconfigured case from the browser's point of view.
