### Fixed

- **cli:** the default, `--api`, `--daemon` and `--bundled-pg` scaffolds now
  write two fixes for the Windows PDB symbol limit (LNK4319, issue #2448). One
  is `[profile.dev] debug = "line-tables-only"` in `Cargo.toml`, for the app
  and its dependencies. The other is `.cargo/config.toml` with
  `/DEBUG:LongSymbolTruncate` for Windows MSVC targets. Backtraces keep file
  and line. Debuggers lose variable inspection: set `debug = true` in both
  profile tables to get it back. Older projects: add the files yourself (see
  the platform-support guide).
