### Fixed

- **deploy:** `autumn deploy status --strict` now reports state drift for a
  deployed host whose installed kamal-proxy unit has no readable `--http-port`
  (issue #2278). The next deploy of that host refuses. Before, `--strict`
  exited 0. A host that is not deployed is not flagged.
