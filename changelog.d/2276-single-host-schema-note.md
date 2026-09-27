### Fixed

- **cli:** a single-host `autumn deploy up` that fails at or after `migrate`
  and before the cutover now says that the migration was not rolled back
  (issue #2276). A redeploy says that the previous release runs on the migrated
  schema. A first deploy says that no release is serving. The remote commands
  and the exit code do not change.
