### Added

- **tls:** end-to-end HTTP/2 tests for the in-process TLS listener (issue
  #2321). The listener already offers ALPN `h2` and `http/1.1`. New tests use
  real h2 and HTTP/1.1 clients. They check ALPN, the h1 fallback, no
  extended CONNECT (so `wss://` stays on HTTP/1.1), the request timeout, SSE
  and graceful shutdown over h2.
