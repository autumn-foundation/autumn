### Changed

- **cli:** `autumn new` writes `[profile.dev] debug = "line-tables-only"`, for
  the app and its dependencies, into the new `Cargo.toml`. A Windows debug
  build of a `--bundled-pg` app no longer hits the PDB symbol limit
  (LNK4319, issue #2448). Backtraces keep file and line.
