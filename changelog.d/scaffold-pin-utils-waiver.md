### Changed

- **new:** the generated `deny.toml` waives `RUSTSEC-2026-0441` (`pin-utils`
  is unmaintained). It reaches an app only through autumn-web's
  `cfg(loom_models)` `loom` dependency, which a normal build never compiles.
  Without the waiver a new app's advisory gate fails on day one.
