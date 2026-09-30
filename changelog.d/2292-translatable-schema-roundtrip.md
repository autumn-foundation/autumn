### Fixed

- **schema:** a `#[translatable]` column no longer reports perpetual drift on
  Postgres (issue #2292). `pg_get_expr` renders the column's stored `'{}'`
  default as `'{}'::text` while the model records the bare literal; the
  introspection side now strips a redundant own-type cast on a literal default
  (`'{}'::text`, `'draft'::text`, `42::integer`), so the round-trip diff is
  empty. A cast to a different type, a cast carrying a typmod, or a cast over
  a non-literal expression is still preserved verbatim.
- **schema:** the declarative parser keys `Translated`-typed fields on the
  `#[translatable]` marker instead of the type name (issue #2292). An
  application type that happens to share the name (e.g. `domain::Translated`)
  without the marker is now skipped with a diagnostic instead of being
  silently managed as a framework text column.
