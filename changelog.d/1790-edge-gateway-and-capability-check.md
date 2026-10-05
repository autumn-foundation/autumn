### Added

- **edge:** `autumn_edge::gateway::EdgeGateway` (feature `host`), a reference
  gateway (issue #1790). It puts a capsule in front of any origin
  `tower::Service`. When the capsule declines a request, the gateway sends the
  original request to the origin and returns the origin's answer unchanged.
  `with_response_headers` sets the origin's security headers on edge responses.
- **edge:** `autumn doctor` has a new `edge_capabilities` check. It fails when
  an `#[edge]` route needs something the edge cannot provide: an unknown
  `needs(...)` capability, a write method, an origin-only extractor such as
  `Db` or `Session`, or `EdgeIdentity` without `needs(identity)`. `autumn
  build` stops on the same error before it compiles.
- **edge:** `conformance::SECURITY_HEADERS` and `conformance::compare_capsule`
  compare a raw capsule response with the origin. Every header other than
  the volatile and security headers must match in both directions.

### Fixed

- **edge:** `autumn build` no longer fails when the app has `#[edge]` routes
  and no static routes. The capsule is then the build's output, so the
  static renderer finding no routes is not an error.

### Testing

- **edge:** the `edge-conformance` CI job sends 10,000 generated requests
  through the native edge lane, the wasm capsule, the origin and the gateway,
  and requires zero divergence. It also runs the real `autumn build` on the
  example and checks that the `.wasm` exists.
