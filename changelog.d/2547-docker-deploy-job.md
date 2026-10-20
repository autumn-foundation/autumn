### Fixed

- **ci:** `deploy_e2e` runs in its own `Test (Docker deploy)` job, on a fresh
  runner disk. The Docker sweep no longer runs the runner out of disk space
  (issue #2547).
