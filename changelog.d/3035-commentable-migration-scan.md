### Fixed

- **cli:** the `comments:commentable` migration scan (issue #3035) now reads
  `archive . comments` as the table `comments` in schema `archive`. It reads
  each `ALTER TABLE` action alone: `ADD COLUMN author_id … REFERENCES users(id)`
  adds `author_id`, not `id`. Before, a table that lacked a required column
  could look like the shared `comments` table, and the generator skipped the
  migration it needs.
