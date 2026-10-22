### Changed

- **generate:** on SQLite, `Remove…From…` finds blocking indexes by replaying
  earlier migrations in memory and asking SQLite (issue #2579). It no longer
  reads SQL text. A column behind an inline `UNIQUE` or `PRIMARY KEY` now
  fails at generate time. An earlier migration that does not replay stops
  `generate migration` and names the file.
