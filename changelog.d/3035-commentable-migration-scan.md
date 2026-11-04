### Fixed

- **cli:** the `comments:commentable` migration scan (issue #3035) now reads
  `archive . comments` as table `comments` in schema `archive`.
- **cli:** the scan now reads each `ALTER TABLE` action alone.
  `ADD COLUMN author_id … REFERENCES users(id)` adds `author_id`, not `id`.
  `ADD CONSTRAINT`, `ADD PRIMARY KEY` and `ALTER COLUMN` add no column.
  Before, a table that lacked a required column could look like the shared
  `comments` table, and the generator skipped the migration it needs.
