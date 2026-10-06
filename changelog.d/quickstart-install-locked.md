### Fixed

- **docs:** the quickstart's `cargo install autumn-cli` now passes `--locked`,
  so it installs on the advertised minimum Rust (1.88.0). Without it, cargo
  re-resolved dependencies and picked up `uuid` 1.27, which needs rustc 1.89.
