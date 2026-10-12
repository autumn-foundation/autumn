### Fixed

- **scaffold:** the `deny.toml` that `autumn new` writes waives
  `RUSTSEC-2024-0449` (`unsafe-libyaml`, unmaintained). The crate arrives
  through `serde_yaml` in `autumn-web`, and no safe upgrade exists. Without the
  waiver, a new app's dependency audit failed on its first push. The review-by
  date is 2026-11-09.
