### Added

- **edge:** `autumn edge serve` runs an edge node (issue #1790). The node puts
  the capsule from `autumn build` in front of a remote origin. It serves what
  the capsule serves and sends every other request to the origin over HTTP.
  At start it copies the origin's static security and CSP headers. It
  does not follow redirects, removes hop-by-hop headers, refuses a path with
  a dot segment, and tunnels WebSocket upgrades to the origin. It replaces
  the client's `x-forwarded-*` headers for both lanes, except from a
  `--trusted-proxy` peer such as a TLS terminator.
  In Rust:
  `autumn_edge::node::EdgeNode` (feature `node`).
- **edge:** `autumn edge ttfb` measures time to first byte at the edge node
  and at the origin, and compares the bytes of each pair. It fails on one
  divergence or on a median reduction below `--min-reduction` (default 50%).
- **edge:** `conformance::CORS_HEADERS`. `compare_capsule` ignores them,
  because the host sets them.
- **edge:** The `edge-conformance` CI job has Tier E: the edge node over real
  HTTP, with a simulated origin round trip of 150 ms, must show an at least
  50% lower median TTFB and zero divergence.
