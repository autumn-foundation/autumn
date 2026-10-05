### Fixed

- **cli:** `autumn generate auth --mail` no longer panics when the target
  project's `Cargo.toml` declares `autumn-web` as an inline table with a
  multiline `features = [...]` array (legal TOML since 1.0). The `mail`
  feature injector now scans forward for the array's real closing `]` the
  same way the `oauth2`/`webauthn` variants already did, instead of
  `.unwrap()`-ing a same-line search that returned `None` (issue #2753).
