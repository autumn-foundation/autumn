### Fixed

- **graph:** a `may_read` or `may_write` entry with a `*` now matches the
  tables in the graph only (issue #3606). Before, the check also read the
  pattern as a table name. So with `owns = ["*"]` on one boundary,
  `may_read = ["campaign*"]` allowed the schema crate of that boundary.
  With no graph, such an entry matches no table and gives a warning. An
  `owns` pattern with a schema (`public.contacts`) no longer gives a false
  "matches no table" warning.
