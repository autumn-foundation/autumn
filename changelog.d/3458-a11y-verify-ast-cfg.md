### Fixed

- **a11y:** `autumn a11y verify` reads `#[cfg]` from a `syn` AST (issue
  #3458). A gated node ends where the Rust grammar ends it. The scan reads a
  macro body as Rust when the body parses as Rust. In `cfg_if!`, it keeps
  only the branch that builds. A `#![cfg(…)]` or a gated `mod name;` that is
  off takes out its code. Only the route attribution changes.

### Changed

- **a11y:** `autumn a11y verify` solves features as `cargo build --workspace`
  solves them (issue #3458). A `--features` name is on only in the crates
  that declare it. A crate turns on the features that it asks for in a path
  dependency of the same workspace. A target table counts only when its `cfg`
  or its triple matches the host. Only the route attribution changes. [no-plugin]
