### Fixed

- **a11y:** `autumn a11y verify` now reads `#[cfg_attr(…, cfg(…))]` on
  statements, match arms and fields (issue #3458). It also finds the end of a
  gated arm or statement that uses a block as an `if`, `while` or `match`
  condition. Findings and the exit code do not change. Only the route
  attribution changes.

### Changed

- **a11y:** `autumn a11y verify` prints a warning when a `--features` name is
  declared by no scanned crate (issue #3458).
