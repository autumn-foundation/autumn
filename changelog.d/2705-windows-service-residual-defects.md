### Fixed

- **serve (Windows):** five residual service defects (issue #2705).
  - `icacls /reset` now runs recursively (`/T /L`), and `install-service` writes the
    service record as a new file. A file that another local user pre-created
    in a shared `AUTUMN_RUNTIME_DIR` no longer keeps its ACE.
  - `serve restart` takes the daemon path only when the service does not
    exist. A denied or failed service query is now an error.
  - `uninstall-service` reads the record path from the service entry and
    cleans the tree the service was installed under.
  - A very large `prestop_grace_secs` or `shutdown_timeout_secs` no longer
    panics the stop path. The stop wait is capped at 24 hours.
  - The preshutdown timeout is set from the budget the app reports, not from a
    guess made at install time. Until then it keeps the OS default.
