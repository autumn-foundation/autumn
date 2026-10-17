### Fixed

- **plugin:** `plugin remove` now drops the dependency when `[[bin]] path = "src/main.rs"`
  is the only user of it (issue #2476).
- **new:** `autumn new --daemon --with <plugin>` now fails. Before, the plugin put the
  database stack back into the DB-free daemon (issue #2476).
