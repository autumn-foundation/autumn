### Fixed

- **cli:** `autumn build` now finds the app binary for the static renderer
  when `build.target` or `CARGO_BUILD_TARGET` is set (issue #2638). It uses
  the `executable` path from the `cargo build` output.
