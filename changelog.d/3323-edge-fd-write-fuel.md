### Fixed

- **autumn-edge:** the reference host now charges fuel for host work in
  `fd_write`, `fd_read` and `random_get` (issue #3323). A capsule can no
  longer force unmetered host work with padded stdout lines, a large `iovs_len`
  or a large `random_get`. Stderr past its cap is no longer copied.
