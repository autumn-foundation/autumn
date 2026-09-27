# CORS and Cross-Origin Requests

If a browser on `https://app.example.com` fetches your Autumn app on
`https://api.example.com`, the browser — not Autumn — decides whether the
JavaScript is allowed to read the response. It decides by looking for
`Access-Control-*` headers. Autumn sends those headers when, and only when, you
list the calling origin in `[cors]`.

This page is the `[cors]` section: what each key does, what the defaults are,
and the two combinations that fail. It assumes you already have a route that
works when you `curl` it, and the problem is a browser on another origin.

If you are calling your app from its own origin — a server-rendered page, an
htmx fragment, a form post — you do not need CORS at all, and enabling it
changes nothing.

## Quick start: enable CORS

CORS is **off by default**. Turn it on by listing the origins allowed to call
you, in `autumn.toml`:

```toml
[cors]
allowed_origins = ["https://app.example.com"]
```

Or via an environment variable, for a deployment override without editing
config files:

```
AUTUMN_CORS__ALLOWED_ORIGINS=https://app.example.com,https://admin.example.com
```

That is the whole common case. The default methods and headers already cover a
JSON API: `GET`, `POST`, `PUT`, `DELETE`, `PATCH`, `OPTIONS`, with
`Content-Type` and `Authorization` accepted on the request.

## Allowed origins, and the empty default

`allowed_origins` is the switch. When it is **empty — the default — the CORS
middleware is not installed at all**: no `Access-Control-*` header is sent on
any response, and a cross-origin browser call fails no matter what the other
four keys say.

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

## CORS with credentials (cookies and `Authorization`)

`allow_credentials = true` makes Autumn send
`Access-Control-Allow-Credentials: true`. **On its own that is not enough to get
your session cookie onto a cross-origin request** — it is one of three
independent conditions, and all three must hold:

```toml
[cors]
allowed_origins = ["https://app.example.com"]   # 1. the server permits credentials
allow_credentials = true
```

2. **The caller must ask for them.** A cross-origin `fetch` sends no cookies
   unless it opts in, and the default (`same-origin`) does not count as opting
   in:

   ```js
   fetch("https://api.example.com/me", { credentials: "include" })
   ```

3. **The cookie must be sendable cross-site.** Autumn's session cookie defaults
   to `session.same_site = "Lax"`, and a `Lax` cookie is not sent on a
   cross-site `fetch` at all — so this combination silently yields an
   unauthenticated request even with the two above in place. A cross-site
   session needs:

   ```toml
   [session]
   same_site = "None"
   ```

   Browsers honor `None` only on a `Secure` cookie. `session.secure` is already
   `true` by default, so there is nothing to flip — but it does mean both the app
   and the calling page must be on HTTPS, which rules the combination out over
   plain `http://localhost` unless you terminate TLS locally.

   That is a real loosening of CSRF protection — `SameSite` is a defense you are
   giving up — so prefer putting both the app and its front-end on one origin
   (a reverse proxy, a path prefix) over reaching for `None`. See
   [Authentication](authentication.md) for the rest of the session cookie's
   settings.

If requests arrive unauthenticated with all three in place, check the request in
devtools for a `Cookie` header: if the browser is not sending one, the problem is
condition 2 or 3, not `[cors]`.

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

A missing `Access-Control-Allow-Origin` does **not** tell you which of two
different problems you have, because both look identical in devtools: the layer
may not be installed (`allowed_origins` is empty), or it may be installed and
the request's `Origin` may simply not match any entry — an exact match on
scheme, host and port, so a differing port or `http` vs `https` misses. Autumn
distinguishes them for you in the startup log: when the layer is installed it
logs `CORS enabled` with the origin list and the credentials flag, and the
startup banner lists `CORS` among the active middleware. No such line means
`allowed_origins` is empty; a line whose list does not contain the origin your
browser is actually sending means the allowlist is the problem.

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
successful preflight, so it is not re-sent before every call. Lower it while
debugging an allowlist, because a cached preflight keeps answering with the old
policy.

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
