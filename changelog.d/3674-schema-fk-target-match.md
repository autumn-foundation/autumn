### Fixed

- **schema:** `autumn schema diff` now uses one rule to match a foreign key
  target to a table. The rule reads the schema of the target (`users`,
  `public.users`, `blog.users`). These errors no longer occur:
  - A type change of `users.id` was refused when an FK pointed at
    `auth.users`.
  - A table rename was refused when an index or table with the new name was
    in another schema.
  - The diff showed a foreign key change when the model wrote
    `blog.authors` and the snapshot wrote `authors`.
  - A table rename did not update a qualified FK (`blog.posts`) to the
    renamed table.

  See [ADR 0047](docs/adr/0047-schema-tools-key-tables-by-name.md).
