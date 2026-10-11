### Fixed

- **upgrade:** `autumn upgrade` now ties CI workflow ownership to the git root,
  not the Cargo workspace (issue #2344). A crate that is its own git root
  (nested repository or submodule) in a Cargo workspace keeps `ci.yml` and
  `posture-gate.yml` in scope, so `--check` catches a stale workflow. The
  Cargo-resolved files (`clippy.toml`, `rustfmt.toml`, `rust-toolchain.toml`)
  stay out of scope. See `docs/guide/upgrading.md`.
