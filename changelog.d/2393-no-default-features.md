### Fixed

- **`autumn sbom --no-default-features` (#2393):** `metadata_args` could only
  *add* features (`--features`, `--all-features`), so `cargo metadata` always
  resolved with the `default` feature on. An app built with `cargo build
  --no-default-features` got an SBOM listing every optional dependency the
  default set pulls in — crates not in the shipped binary. The subcommand now
  takes `--no-default-features` (conflicting with `--binary`, like the other
  resolution switches) and forwards it to `cargo metadata`, where it composes
  with `--features` for a slimmed build with a few extras on top.
