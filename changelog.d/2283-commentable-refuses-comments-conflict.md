### Changed

- **generate:** `comments:commentable` now stops with an error when the project
  already has a plain `comments` table (issue #2283). It used to warn and write
  a second `CREATE TABLE comments`. No file is written. Rename the `Comment`
  resource, or add the polymorphic columns to its table, then run it again.
