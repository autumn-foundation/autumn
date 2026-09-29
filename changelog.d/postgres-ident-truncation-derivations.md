### Fixed

- **Derivation registry checks compare the Postgres physical identifier
  spelling (issue #2664):** Postgres truncates every identifier to
  `NAMEDATALEN - 1` bytes (63 on a stock build), and quoting does not exempt
  it — but the derivation/counter-cache collision guard compared full
  spellings. Two maintained-column claims that agreed on their first 63 bytes
  passed the boot check as distinct while the database saw one column, so
  mutations would have double-applied deltas and backfills overwritten each
  other. The Postgres registry key is now the spelling truncated to 63 bytes
  on a char boundary (SQLite is unchanged: full spelling, ASCII-case-folded).
  `docs/guide/derivations.md` documents the bound, and new tests cover the
  truncation, the multi-byte char boundary, the rejection, and the SQLite
  mirror case.
