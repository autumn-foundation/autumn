### Fixed

- **deploy:** `autumn deploy status --strict` now reports state drift for a
  deployed host whose installed kamal-proxy unit has no readable `--http-port`
  (issue #2278). The next deploy refuses that host. Before this fix, `--strict`
  exited 0. This reason does not apply to a host that is not deployed.
