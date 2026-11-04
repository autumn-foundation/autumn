### Fixed

- **build scripts:** a relative `CARGO_TARGET_DIR` no longer makes the
  Tailwind lookup use a stale binary in the crate directory (issue #3108).
  The scaffolded `build.rs`, the examples, and the starters now skip a
  relative value. They use the `OUT_DIR` ancestors instead.
