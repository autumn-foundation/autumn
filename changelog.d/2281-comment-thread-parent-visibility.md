### Fixed

- `commentable`: `comment_thread` re-evaluates the parent visibility rule
  (existence, `deleted_at` liveness, the caller's tenant) as an `EXISTS`
  inside its own read statement. Previously the probe and the read were two
  statements with two snapshots under read-committed, so a parent
  soft-deleted or moved to another tenant in the window between them had its
  thread served to a caller no longer entitled to see it
  ([#2281](https://github.com/autumn-foundation/autumn/issues/2281)). The
  probe is kept, so a missing or invisible parent still returns `404`; a
  parent that disappears between the probe and the read now returns an empty
  thread instead.
