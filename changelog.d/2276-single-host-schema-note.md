### Fixed

- **cli:** a single-host `autumn deploy up` that fails after `migrate` and
  before the cutover now says that the migration was not rolled back
  (issue #2276). A redeploy names the previous release on the migrated schema.
  A first deploy says that nothing serves and the schema has moved. The fleet
  schema-note rules pick the line. The exit code and the ops do not change.
