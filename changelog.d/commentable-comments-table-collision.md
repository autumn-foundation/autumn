### Changed

- **cli:** `autumn generate scaffold|model … comments:commentable` now refuses
  when the project has a `comments` table that is not the shared polymorphic
  one, for example from a `Comment` scaffold (issue #2283). Before, it wrote a
  second `CREATE TABLE comments` migration and `autumn migrate` stopped on
  "already exists". The error names the missing columns and the remedy, and no
  file is written.
