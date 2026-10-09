### Added

- **tls [no-plugin]:** end-to-end HTTP/2 tests for the in-process TLS listener (issue
  #2321). The tests use real h2 and HTTP/1.1 clients. They check that ALPN
  picks `h2`, that HTTP/1.1 clients still work, and that the request timeout,
  SSE, graceful shutdown and `wss://` work over h2. CI runs them in the `tls`
  lane.

### Fixed

- **ws [no-plugin]:** `#[ws]` routes now accept the HTTP/2 `CONNECT` upgrade (RFC 8441)
  as well as `GET` (issue #2321). Before, a browser on `h2` got `405` for
  `wss://`. CSRF, captcha and read-only mode now let this upgrade pass, like
  the `GET` upgrade.
