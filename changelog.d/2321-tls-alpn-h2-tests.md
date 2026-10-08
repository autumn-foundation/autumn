### Added

- **tls:** end-to-end HTTP/2 tests for the in-process TLS listener (issue
  #2321). The listener already offers ALPN `h2` and `http/1.1`. New tests use
  real h2 and HTTP/1.1 clients. They check ALPN, the h1 fallback, no
  extended CONNECT (so `wss://` stays on HTTP/1.1), the request timeout, SSE
  and graceful shutdown over h2.

### Fixed

- **ws:** `#[ws]` routes now accept the HTTP/2 `CONNECT` upgrade (RFC 8441)
  as well as `GET` (issue #2321). Before, a browser that negotiated `h2`
  got `405` for `wss://`.
