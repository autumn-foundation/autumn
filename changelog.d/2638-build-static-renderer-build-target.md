### Fixed

- **cli:** `autumn build` now finds the app binary for the static renderer
  when `build.target` or `CARGO_BUILD_TARGET` is set (issue #2638). It uses
  the `executable` path from the `cargo build` output, as the other commands
  do since issue #2374.
- **cli:** when `build.target` names more than one triple, the CLI starts the
  binary for the host triple (issue #2638). Before, it started the binary
  that Cargo reported last, which can be for a different machine.
