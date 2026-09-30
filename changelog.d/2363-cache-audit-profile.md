### Fixed

- **cli:** `autumn cache audit` accepts `--release` and `--profile <NAME>`,
  builds the audited binary under that profile, and reads it from the
  profile's own `target/` directory (`target/release`, `target/<NAME>`, with
  `--profile dev`/`test` mapping to `target/debug` and `bench` to
  `target/release`). A `#[cached]` read behind
  `#[cfg(not(debug_assertions))]` was never compiled into the debug build the
  audit always used, so the gate could pass on a build nobody deploys. The
  audit header names the profile alongside the feature selection
  (issue #2363).
