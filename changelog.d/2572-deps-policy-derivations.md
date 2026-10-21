### Fixed

- **doctor:** `autumn doctor` reports no verdict when its check list and the
  CI workflow's check list differ in either direction, such as a section
  header quoted inside a TOML string (issue #2572).
- **doctor:** a relative `db-path` in `deny.toml` is read from the policy
  directory, so the advisory data age shows from a workspace member too
  (issue #2572).
