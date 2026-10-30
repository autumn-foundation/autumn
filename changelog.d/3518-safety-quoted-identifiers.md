### Fixed

- **cli:** `autumn migrate check` no longer misses statements after a quoted
  identifier that holds `/*`, `--`, `;` or `'`, such as `"a/*b"` (issue
  #3518). The scanner now reads the quotes of the app's backend: Postgres
  `".."`, `E'..'` and `$tag$..$tag$`; `SQLite` `".."`, `` `..` `` and `[..]`.
  A `,` in a quoted value no longer splits a Postgres `ALTER TABLE` into a
  false "Unclassified ALTER TABLE" finding.
