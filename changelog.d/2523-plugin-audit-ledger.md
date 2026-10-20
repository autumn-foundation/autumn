### Fixed

- **plugin sandbox:** the activity log keeps each call's own time, never loses
  dropped-call counts when its ledger fills, and reads both ledgers under one
  lock. A failed DB call no longer counts as a table touched (issue #2523).
