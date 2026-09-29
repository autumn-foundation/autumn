### Fixed

- **cli:** a single-host `autumn deploy up` that fails at or after `migrate`
  and before the cutover now says that the migration was not rolled back
  (issue #2276). A redeploy says that the previous release runs on the migrated
  schema. A first deploy says that no release is serving. The remote commands
  and the exit code do not change.
- **cli:** the step that repairs a drifted live-slot marker before a redeploy
  is now labelled `repair-live-slot`, not `record-live-slot`. It runs before
  `migrate`, so if it fails, the single-host error and the fleet summary no
  longer say that the schema moved.
