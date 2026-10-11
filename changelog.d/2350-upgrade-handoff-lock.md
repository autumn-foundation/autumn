### Fixed

- **upgrade:** In-place upgrade no longer kills a successor that is publishing
  readiness (issue #2350). Both processes now take a lock in the handoff
  directory for that step. A successor that finds no lock file refuses to start.
