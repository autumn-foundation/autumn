### Fixed

- **graph:** a `may_read` or `may_write` entry with a `*` in its table part
  now matches only the tables in the graph: the tables of the models and
  repositories (issue #3606). Before, the check also read the pattern as a
  table name. When one boundary had `owns = ["*"]`,
  `may_read = ["campaign*"]` also allowed the schema crate of that boundary.
  With no graph, such an entry matches no table, so it allows no
  dependency. The check gives a warning: write the table name, or check a
  graph. An `owns` pattern with a schema (`public.contacts`), or one that
  matches only a repository table, no longer gives a false "matches no
  table" warning.
