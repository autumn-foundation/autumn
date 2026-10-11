### Fixed

- **db:** `DatabaseFleet::delete` and `restore` of an existing SQLite database
  now work on Windows (issue #3227). A database that another process opens
  during the removal gives `409`.
