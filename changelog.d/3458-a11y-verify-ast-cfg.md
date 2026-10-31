### Fixed

- **a11y:** `autumn a11y verify` now reads `#[cfg]` from a `syn` AST, so a
  gated node ends where the Rust grammar ends it (issue #3458). A macro body
  that parses as Rust is read the same way. A `#![cfg(…)]` that is off takes
  out its file, module or function. Only the route attribution changes.
- **a11y:** `autumn a11y verify` solves features as
  `cargo build --workspace` does (issue #3458). A `--features` name is on only
  in the crates that declare it. A crate of the scan turns on the features
  that it asks for in a path dependency that is in the scan.
