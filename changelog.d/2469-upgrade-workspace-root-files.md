### Added

- **cli:** `autumn upgrade` now adopts the root files for a Cargo workspace
  member (issue #2469). It plans `clippy.toml`, `rustfmt.toml` and
  `rust-toolchain.toml` at the workspace root, and `ci.yml` and
  `posture-gate.yml` at the git root, as `../` paths. The workflows name the
  member. A root file that exists already is a conflict, and the command does
  not overwrite it. Use `--accept ../<file>` to leave it as it is.

### Changed

- **release:** the deploy workflows read the posture manifest path from
  `POSTURE_MANIFEST`. They print a `::warning::` when the file is missing.
  Before, they skipped the attestation with no message.
