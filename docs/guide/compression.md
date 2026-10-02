# Response Compression

Autumn provides first-class, configurable response compression via a built-in
[`CompressionLayer`][cl] that honors the client's `Accept-Encoding` header.

[cl]: https://docs.rs/tower-http/latest/tower_http/compression/struct.CompressionLayer.html

## Quick start

Compression is **off by default**. Enable it with one line in `autumn.toml`:

```toml
[compression]
enabled = true
```

Or via an environment variable (useful for deployment overrides without changing
config files):

```
AUTUMN_COMPRESSION__ENABLED=true
```

That is all. Autumn's middleware stack automatically applies gzip or brotli
compression to all compressible responses — HTML, JSON, CSS, JavaScript, SVG,
XML, and plain text — based on what the client advertises in `Accept-Encoding`.

## Supported algorithms

| Algorithm | Feature flag |
|-----------|-------------|
| gzip / deflate | always available |
| brotli (`br`) | always available |

The `CompressionLayer` negotiates the best available algorithm automatically.

## What gets compressed

The layer uses standard content-type detection. Compressible types include:

- `text/html`
- `application/json`
- `text/css`
- `text/javascript`, `application/javascript`
- `image/svg+xml`
- `application/xml`, `text/xml`
- `text/plain`

Binary types (images, audio, video, archives) and responses that already carry a
`Content-Encoding` header are passed through unchanged — no double-compression.

## `Vary: Accept-Encoding`

Autumn sets `Vary: Accept-Encoding` on all compressible responses so that HTTP
caches store separate entries per encoding. This is done automatically; no extra
configuration is needed.

## ETag compatibility

Autumn's compression layer is placed **outside** any user-registered `EtagLayer`
in the middleware stack. This means:

1. `EtagLayer` (or `fresh_when()`) computes the ETag on the **uncompressed** body.
2. The compression layer then encodes the body for transit.
3. The `Vary: Accept-Encoding` header ensures caches key entries by encoding.

Weak ETags (`W/`) are safe to use with compression per RFC 7232 §2.1 — weak
comparison explicitly allows encoding variations. When using explicit strong ETags
(via `fresh_when()`), the ETag is still computed on the content before encoding,
which is the correct semantic: the ETag identifies the *resource*, not the
*representation encoding*.

## Security: BREACH / CRIME tradeoff

> **Read this section before enabling compression in production.**

HTTP compression can leak secrets through a *compression oracle* attack (BREACH,
CRIME). The attack works by injecting attacker-controlled bytes into a response
that is compressed alongside a secret (such as a CSRF token). By measuring how
the compressed length changes, an attacker can recover the secret bit-by-bit.

**When compression is safe to enable:**

- Your CSRF tokens or other secrets are not reflected in *dynamic, user-visible
  response bodies*. Most apps fall into this category: secrets live in cookies or
  HTTP headers, not HTML.
- Your app uses per-request, nonce-based CSRF tokens (Autumn's default) rather
  than long-lived tokens — each guess is burned.
- You do not reflect attacker-controlled query params or form fields verbatim into
  the same response that carries a CSRF token.

**When to prefer CDN / reverse-proxy compression instead:**

- Your responses regularly combine user-controlled input with secrets in the same
  HTML fragment (e.g. a search results page that echoes the query alongside a
  hidden form token).
- You want zero application-level surface area for timing attacks.
- Your hosting platform (Fly.io, Railway, Cloudflare, etc.) already compresses
  at the edge.

Autumn follows the same convention as Rails (`Rack::Deflater`), Django
(`GZipMiddleware`), and Phoenix: compression is a documented, one-line opt-in,
off by default, with the BREACH tradeoff explicitly surfaced here.

## `autumn doctor`

When you run `autumn doctor` in a production profile with compression disabled,
you will see a warning:

```
⚠️  compression — response compression is disabled in production; text payloads
                   (HTML/JSON/CSS) are served uncompressed
   Hint: Set [compression] enabled = true in autumn.toml (or
         AUTUMN_COMPRESSION__ENABLED=true) if you are not using a CDN or
         reverse-proxy that compresses for you. Read the BREACH/CRIME tradeoff
         in docs/guide/compression.md before enabling.
```

This is a *warning*, not a failure — running without compression is a legitimate
choice (CDN compression, single-page app with pre-compressed assets, etc.). The
doctor check is informational only.

## Interaction with static asset serving

The `CompressionLayer` wraps the entire router, so static files under `/static`
(fingerprinted CSS/JS etc.) **are** compressed on-the-fly when the client
advertises `Accept-Encoding` and the content type is compressible. Images are
excluded by `DefaultPredicate` and are served uncompressed regardless.

If you prefer to serve pre-compressed sidecar files (`.gz` / `.br`) and skip
on-the-fly CPU work, generate them during your build and configure `ServeDir`
with `precompressed(true)`. When a sidecar is served, `ServeDir` sets
`Content-Encoding` directly and the `CompressionLayer` skips re-encoding (the
double-compression guard applies). See issue #752 for the static-first SSG/ISG
path.

## Configuration reference

```toml
[compression]
# Enable gzip/brotli response compression for dynamic handlers.
# Off by default — read docs/guide/compression.md before enabling.
enabled = false
```

| Environment variable | Type | Default |
|----------------------|------|---------|
| `AUTUMN_COMPRESSION__ENABLED` | `bool` | `false` |
