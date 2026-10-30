### Fixed

- **db scrub:** on SQLite, the row key is now unique for text keys with NUL
  bytes and for tables with a column named `rowid` (issue #3378). A table that
  shadows `rowid`, `_rowid_` and `oid` is refused with a clear error. Tokens
  for SQLite rows change once, so a scrub gives new fake values after upgrade.
