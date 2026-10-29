### Fixed

- **autumn-edge:** the reference host now charges fuel for `fd_write`
  (issue #3323). It charges the host copy and the stdout line parse. A
  capsule can no longer force unmetered host work with padded stdout lines.
  Stderr past its cap is no longer copied.
