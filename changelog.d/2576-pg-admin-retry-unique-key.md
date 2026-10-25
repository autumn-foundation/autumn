### Fixed

- **jobs:** Postgres admin retry restores a `unique_for = "pending"` key
  (issue #2576). An equal enqueue stays deduplicated while the retry waits.
