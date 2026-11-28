# ADR 0053: One iroh Endpoint per Process, and the Cluster on It

- Status: Accepted
- Date: 2026-11-28
- Deciders: Autumn maintainers
- Tags: cluster, networking, security, iroh

## Context

Issue [#3818](https://github.com/autumn-foundation/autumn/issues/3818).

The cluster dials peers by `ip:port`. A node that its peers cannot dial
(behind NAT, or on a container network that publishes one port) makes a
one-way cluster: it sends pushes, and it never gets any back.

iroh 1.5 gives a process a QUIC endpoint whose address is an ed25519 public
key. The handshake proves the key. With a relay, iroh goes through NAT.

## Options

1. Use `iroh::protocol::Router`. Rejected: it owns the accept loop, and we
   need a per-ALPN allow-list and a bound on handshakes in progress.
2. Use iroh with its default features. Rejected: `portmapper` and `metrics`
   add crates that the default config does not use.
3. One `IrohNode` per process, with its own ALPN registry and accept loop,
   on `iroh` with `default-features = false, features = ["tls-ring"]`.

## Decision

Option 3.

- `[iroh]` binds one endpoint. The secret key file is created `0600`. A file
  that others can read is refused.
- The relay is off and the address lookup is static by default. The
  endpoint then talks to no third party. `n0` relays, custom relays and n0 DNS
  are opt-ins.
- Each protocol registers an ALPN. An unknown ALPN fails the handshake. A
  per-ALPN allow-list closes other peers after the handshake, before a
  handler sees them. `[iroh] allowed_peers` is the list for every ALPN.
- `[cluster] transport = "iroh"` uses ALPN `autumn/cluster/1`. One QUIC uni
  stream carries one frame. The envelope, the MAC, the queues and the
  farewell lane do not change.
- One connection to a peer carries frames in both directions. The writer
  uses an open connection before it dials. This removes the one-way cluster.
- `PeerTransport` gets three hooks with defaults that keep TCP as it was:
  `advertise_addr`, `dial_targets` and `verify_sender`.
- `verify_sender` refuses a frame whose sender's own record names another
  endpoint id than the connection (`reason="sender"`). It does not stop a
  holder of the secret from gossiping false records about other nodes. The
  victim refutes those at a higher incarnation. Per-record signatures would
  stop it, and are out of scope.
- The endpoint closes after the cluster departs, because the farewell goes
  on it.

## Consequences

- The feature is additive and off by default (`iroh`).
- ring stays the only crypto backend.
- A node with a static `node_id` keeps its id when its key changes. Its new
  record wins at a higher incarnation, as an address change does on TCP.
- Serving the app over iroh and the `iroh://` client scheme build on
  `IrohNode` in later work.
