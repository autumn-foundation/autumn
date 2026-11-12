### Fixed

- **schema:** `autumn schema diff` matches a foreign key target to a table in
  one schema-aware way (`users`, `public.users` and `blog.users`). These false
  refusals are gone:
  - a type change of `users.id` when an FK points at `auth.users`;
  - a rename when an index or table of the new name is in another schema;
  - an FK that the model writes `blog.authors` and the snapshot writes
    `authors`.

  A rename now also updates a qualified FK to the renamed table. See
  [ADR 0047](docs/adr/0047-schema-tools-key-tables-by-name.md).
