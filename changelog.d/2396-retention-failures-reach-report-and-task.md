### Fixed

- **retention:** a failed audit write now sets the dataset `error` (issue
  #2396). `autumn db retention --purge` exits non-zero, and the report shows
  the failure. The `autumn-retention-sweep` task now fails when any dataset
  reports an error, after all datasets ran. Task health and failure alerts now
  show the fault.
