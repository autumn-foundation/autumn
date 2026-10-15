### Added

- **cli:** `autumn fleet list | migrate | backup | delete | restore` operates a
  SQLite database fleet from the command line (issue #3232). It needs the
  `sqlite` build of the CLI. See "Operating the fleet" in
  `docs/guide/sqlite-fleet.md`.
- **db:** `DatabaseFleet::migrate` migrates one database.
  `DatabaseFleet::inspect` reads the identity row and the applied migrations
  (`FleetDbInfo`) and writes nothing. `DatabaseFleet::remapped_migrations`
  lists migrations recorded under a substitute version,
  `known_migration_versions` the versions the fleet's sets record, and
  `check_control_outside_root` refuses a control database inside the root. The builder gets
  `migration_source` and `framework_migrations`.
