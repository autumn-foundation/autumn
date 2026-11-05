### Fixed

- **cli:** `autumn migrate check` reads non-ASCII text as Postgres and SQLite
  do (issue #3530). Each non-ASCII character is part of a name, also U+00A0
  and other Unicode spaces. Only ASCII letters fold case, so `ÜSERS` and
  `üsers` are two tables. Before, `ADD COLUMN a<U+00A0>$x$ int, ADD COLUMN b
  int NOT NULL` lost the `NOT NULL` finding, and `DEFAULT f$()` was not
  reported as a volatile default.
