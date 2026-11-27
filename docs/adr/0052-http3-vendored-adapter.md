# ADR 0052: HTTP/3 Through a Vendored, Service-Generic h3 Adapter

- Status: Accepted
- Date: 2026-11-27
- Deciders: Autumn maintainers
- Tags: server, tls, http3, security

## Context

Issue [#2322](https://github.com/autumn-foundation/autumn/issues/2322).

Autumn terminates TLS in-process (`[server.tls]`, ACME). An operator who runs
the app directly on the internet wants HTTP/3 without a proxy.

`axum-h3` 0.0.6 serves an `axum::Router` over `h3`. It has two blockers:

1. It takes a `Router`, not a `tower::Service`. Autumn wraps the router in
   `TrustedProxiesLayer` and `MethodOverrideLayer` at the serve boundary.
   Over `axum-h3` both layers would be missing, and `_method=PUT` forms
   would break.
2. It stamps no `ConnectInfo`. Trusted-proxy resolution trusts every peer
   when the peer address is missing, so an untrusted client could spoof
   `X-Forwarded-For` over h3.

## Options

1. Use `axum-h3` as it is. Rejected: blocker 2 is a security hole.
2. Send the fixes upstream and wait for a release. Rejected: a 0.0.x crate
   with one maintainer would gate a security-relevant path.
3. Vendor the adapter into `autumn/src/http3.rs`, on `h3` and `h3-quinn`
   directly, without `h3-util`.

## Decision

Option 3.

- `Http3Listener::serve` takes any `tower::Service<Request<Body>>`. The app
  passes the service that the TCP listener serves.
- Each request carries `ConnectInfo<TlsConnectInfo>` from the QUIC
  connection, then `ClientIdentityLayer` stamps `ConnectInfo<SocketAddr>`, as
  on the TLS listener.
- `ServerTlsParts` holds the provider, the certificate resolver and the
  client verifier. The TCP and the QUIC rustls configs come from one value,
  so a renewal or a CA rotation reaches both.
- The QUIC config is TLS 1.3 only, ALPN `h3`, no early data.
- h3 runs as a task beside the TLS listener, inside the same server task, on
  the same shutdown token. It is not a new `BoundListener` variant.
- Extended CONNECT, WebTransport and QUIC datagrams stay off.
- `h3` 0.0.8 reads a whole HEADERS frame before it checks its size. A
  transport wrapper (`http3/limited.rs`) limits the bytes that `h3` reads
  before a request head is parsed, and on control streams.
- The request head has a time limit. Idle connections close. A drain ends
  after the same grace that TCP connections get.

## Consequences

- The feature is additive and off by default (`http3`).
- Autumn owns the adapter code and its tests. Changes to `h3` need a review
  of `http3.rs`.
- Both changes (service-generic, connect info) are worth upstreaming to
  `axum-h3`.
- quinn uses `rustls-ring`, so `autumn-web` keeps one crypto backend.
