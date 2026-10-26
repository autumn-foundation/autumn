### Fixed

- **cli:** `autumn build` now finds the app binary for the static renderer
  when `build.target` or `CARGO_BUILD_TARGET` is set (issue #2638). It uses
  the `executable` path from the `cargo build` output. The other commands
  use this path since issue #2374.
- **cli:** when `build.target` names more than one triple, the CLI starts the
  binary whose arch and OS agree with the machine (issue #2638). Before, it
  started the binary that Cargo reported last, which can be for a different
  machine.
