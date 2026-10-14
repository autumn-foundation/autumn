### Fixed

- **cli:** `autumn serve` and the one-shot commands now find the app binary
  when `build.target` is set (issue #2374). The CLI takes the `executable`
  path from the `cargo build` output. It does not build the path itself.
