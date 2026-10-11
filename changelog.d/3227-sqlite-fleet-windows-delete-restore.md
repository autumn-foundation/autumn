### Fixed

- **db:** `DatabaseFleet::delete` and `restore` of an existing SQLite database
  now work on Windows (issue #3227). If another process opens the database
  during the removal, the call fails with `409`.
