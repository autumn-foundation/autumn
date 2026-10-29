### Fixed

- **build scripts:** a relative `CARGO_TARGET_DIR` no longer makes the
  Tailwind lookup pick a stale member-local binary (issue #3108). The
  scaffolded `build.rs`, the examples, and the starters now use only an
  absolute value and fall back to the `OUT_DIR` ancestors.
