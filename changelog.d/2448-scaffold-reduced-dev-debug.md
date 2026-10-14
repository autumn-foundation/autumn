### Fixed

- **cli:** the default, `--api`, `--daemon` and `--bundled-pg` scaffolds now
  write `[profile.dev] debug = "line-tables-only"` into `Cargo.toml`. It also
  sets this for dependencies. A Windows debug build of a `--bundled-pg` app no
  longer hits the PDB symbol limit (LNK4319, issue #2448). Backtraces keep file
  and line. Debuggers lose variable inspection: set `debug = true` in both
  tables to get it back. Older projects: add the lines yourself (see the platform-support
  guide).
