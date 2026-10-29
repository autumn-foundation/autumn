### Fixed

- **billing:** a store error while dunning ends or recovers no longer strands
  the row (issue #3090). The retry job now writes the mirror first and settles
  the row last, so the next run repeats the write. A recovery that races
  exhaustion undoes the `Unpaid` status.
