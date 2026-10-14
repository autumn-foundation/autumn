### Fixed

- **cli:** one-shot commands and `autumn serve` find the app binary where
  Cargo wrote it, also when `build.target` is set (issue #2374). The CLI
  reads the `executable` path from `cargo build` output. It no longer
  rebuilds the path, so it cannot run a stale binary.
