### Fixed

- **sbom:** `autumn sbom --binary` no longer claims a crates.io purl for
  alternate-registry packages, and no longer emits an empty `vcs_url=` for git
  packages (issue #2386). These packages now have no purl. `cargo:source` still
  records the origin.
