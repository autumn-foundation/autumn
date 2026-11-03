### Fixed

- **fleet:** a slot claim that ends after its time limit is kept, not
  dropped (issue #3379). Shutdown now releases that range, so a planned
  handoff does not wait for the lease to end.
