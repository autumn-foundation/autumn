### Fixed

- **dev, doctor:** an audit that reaches its deadline now kills the `cargo-deny`
  process (issue #2575). On Unix, it also kills the processes that
  `cargo-deny` started. Before, the audit kept running and could hold Cargo's
  package-cache lock. A finished audit is never killed.
- **doctor:** without `cargo-deny`, the dependency check now says that doctor
  cannot check `deny.toml`. CI can still reject a policy that breaks the
  `cargo-deny` schema.
