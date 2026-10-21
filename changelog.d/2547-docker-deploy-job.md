### Fixed

- **ci:** `deploy_e2e` runs in its own `Test (Docker deploy)` job, on a fresh
  runner disk. The Docker sweep no longer fills the runner disk
  (issue #2547).
