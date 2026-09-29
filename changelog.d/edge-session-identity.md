### Added

- **edge:** host-only edge identity provider contract, a session-backed
  identity projection, normalized identity claim types (`EdgeIdentity`,
  `EdgeUserId`, `EdgeRole`, `EdgeIdentityRequired`), and ADR-0005 for the
  security boundary between the host and an edge capsule. A handler taking
  `EdgeIdentity` declares `#[edge(needs(identity))]`, and an unauthenticated
  request falls through to origin before dispatch.

### Documentation

- Documented Redis-compatible DragonflyDB and Valkey deployments.
