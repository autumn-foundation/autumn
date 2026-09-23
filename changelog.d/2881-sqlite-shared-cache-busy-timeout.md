### Documentation

- **`db`:** the `database.statement_timeout` rejection no longer claims
  "`busy_timeout` already bounds lock waits" for `cache=shared` targets —
  shared-cache table-lock conflicts return `SQLITE_LOCKED` without consulting
  the busy handler at all, so the pooled `busy_timeout` does not bound those
  waits (issue #2881). The `docs/guide/money.md` retry advice now documents
  the instant all-contenders-lose failure mode under real contention and its
  mitigations (backoff retry loop, an up-front `IMMEDIATE` write lock, or a
  WAL-mode file database).
