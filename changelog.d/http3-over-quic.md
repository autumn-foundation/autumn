### Added

- **http3:** serve HTTP/3 over QUIC beside the `[server.tls]` listener
  (issue #2322). Build with the `http3` feature and set
  `[server.http3] enabled = true`. QUIC shares the TLS certificate resolver,
  so renewals and mTLS apply to it. TCP responses advertise h3 with
  `Alt-Svc`. h3 requests go through the same `TrustedProxiesLayer` and
  `MethodOverrideLayer` as TCP requests. `autumn doctor` has a new `http3`
  check. See `docs/guide/tls.md`.
