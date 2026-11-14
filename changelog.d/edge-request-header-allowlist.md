### Security

- **edge:** only allowlisted request headers reach an edge capsule. Before,
  the hosts removed only `cookie`, `authorization` and `proxy-authorization`.
  An identity-aware proxy assertion, for example `cf-access-jwt-assertion`,
  `x-amzn-oidc-data`, `x-ms-client-principal` or `x-goog-iap-jwt-assertion`,
  got to the capsule, and the capsule could echo it. Now the Rust gateway,
  the JS host, the guest runtime and the native `#[edge]` mount keep only
  `ALLOWED_REQUEST_HEADERS`: content negotiation, conditional requests,
  `cache-control` and `user-agent`. The origin still gets all headers.

### Breaking Changes

- **Breaking:** `autumn_edge::wire::SENSITIVE_HEADERS` and
  `strip_sensitive_headers` are removed. Use `ALLOWED_REQUEST_HEADERS`,
  `request_header_allowed` and `allowed_request_headers`. An `#[edge]`
  handler that reads a header not on the list through `HeaderMap` does not
  see it on either lane
  ([migration guide](docs/migrations/next.md#edge-handlers-see-only-allowlisted-request-headers)).
