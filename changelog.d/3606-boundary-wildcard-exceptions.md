### Fixed

- **graph:** a `may_read` or `may_write` entry with a `*` in its table part
  now matches the tables in the graph only (issue #3606). Before, the check
  also read the pattern as a table name. When one boundary had
  `owns = ["*"]`, `may_read = ["campaign*"]` also allowed the schema crate
  of that boundary. With no graph, such an entry matches no table, so it
  allows no dependency. The check gives a warning: write the table name, or
  check a graph. An `owns` pattern with a schema (`public.contacts`) no
  longer gives a false "matches no table" warning.
