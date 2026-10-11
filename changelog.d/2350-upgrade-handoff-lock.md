### Fixed

- **upgrade:** In-place upgrade no longer kills a successor that is publishing
  readiness (issue #2350). Both processes now take a lock in the handoff
  directory for that step. A predecessor built before this fix has no lock
  file. The successor then publishes without the lock.
