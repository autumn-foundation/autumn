### Fixed

- **cli:** `autumn migrate check` no longer misses statements after a quoted
  identifier that holds `/*`, `--`, `;` or `'`, such as `"a/*b"` (issue
  #3518). The scanner now uses the lexer rules of the app's backend:
  Postgres `".."`, `E'..'`, case-sensitive `$tag$..$tag$` and nested
  `/* */` comments; SQLite `".."`, `` `..` ``, `[..]` and `$name(..)`
  variables. A `,` in a Postgres literal or quoted identifier no longer splits
  an `ALTER TABLE` into a false "Unclassified ALTER TABLE" finding.
