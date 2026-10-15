### Fixed

- **cli:** `autumn upgrade` now adopts the root-level files for a Cargo
  workspace member (issue #2469). It plans `clippy.toml`, `rustfmt.toml` and
  `rust-toolchain.toml` at the workspace root, and `ci.yml` and
  `posture-gate.yml` at the git root, as `../` paths. The workflows name the
  member. A root file that another member wrote is a conflict, not an
  overwrite.
- **release:** the deploy workflows read the posture manifest path from
  `POSTURE_MANIFEST`, and print a `::warning::` when the file is missing. They
  no longer skip the attestation without a message.
