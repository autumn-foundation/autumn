### Fixed

- **Security:** enforced `#[edge(needs(identity))]` on native origin mounts as
  well as capsule dispatch, preventing unauthenticated edge fallthrough or
  direct-origin requests from reaching declaration-only identity routes.
