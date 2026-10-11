### Fixed

- **upgrade:** `autumn upgrade` now ties CI workflow ownership to the git root
  (issue #2344). A crate that is its own git root, such as a nested repository
  or a submodule in a Cargo workspace, keeps `ci.yml` and `posture-gate.yml` in
  scope. `--check` now catches a stale workflow there. `clippy.toml`,
  `rustfmt.toml` and `rust-toolchain.toml` stay out of scope. See
  `docs/guide/upgrading.md`.
