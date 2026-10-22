### Fixed

- **doctor:** `autumn doctor` gives no verdict when it and the CI workflow
  list different checks for `deny.toml`. One case is a section header quoted
  inside a TOML string (issue #2572).
- **doctor:** a relative `db-path` in `deny.toml` is relative to the directory
  that holds `deny.toml`. `autumn doctor` now shows the advisory data age when
  you run it from a workspace member (issue #2572).
