### Fixed

- **cli:** `autumn generate scaffold|model … comments:commentable` now stops
  with an error when the project has a `comments` table that is not the shared
  one (issue #2283). A `Comment` scaffold makes such a table. Before, the
  generator wrote a second `CREATE TABLE comments`, and `autumn migrate`
  stopped on "already exists". The error names the missing columns and the
  steps to correct it. The generator writes no files. A `Comment` model
  generated after the shared table exists is refused for the same reason.
- **cli:** `autumn generate scaffold … comments:commentable` now plans the
  shared `comments` migration once and prints its notes once.
