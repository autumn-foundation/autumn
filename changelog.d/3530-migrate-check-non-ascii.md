### Fixed

- **cli:** `autumn migrate check` reads non-ASCII text as Postgres and SQLite
  do (issue #3530). Each non-ASCII character, U+00A0 included, is part of a
  name. Only ASCII letters fold case, so `ÜSERS` and `üsers` are two tables.
  Before, `ADD COLUMN a<U+00A0>$x$ int, ADD COLUMN b int NOT NULL` lost the
  `NOT NULL` finding. `DEFAULT f$()` is now a volatile default.
  `DEFAULT 'a(b'` is no longer one.

- **cli:** `autumn migrate check` no longer misses statements after these
  forms (issue #3530). Each one hid a later `DROP TABLE`:
  - a Postgres `E'..'` literal continued on the next line (`E''` then
    `'\''`);
  - a SQLite variable with `::` in its name (`$a::(x')`);
  - a SQLite `@`, `:` or `#` variable right after a word (`SELECT@a(x')`).

  A SQLite default such as `('a' || random())` is now reported as not
  constant. The `-- autumn-safety: reviewed` marker in a string literal no
  longer suppresses the statement.
