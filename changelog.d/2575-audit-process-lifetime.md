### Fixed

- **dev, doctor:** an audit that reaches its deadline now kills the `cargo-deny`
  process and its children (issue #2575). Before, it kept running and could
  hold Cargo's package-cache lock. A finished audit is never killed.
- **doctor:** without `cargo-deny`, the dependency check now says it did not
  validate `deny.toml` either. CI can still reject a policy that breaks the
  `cargo-deny` schema.
