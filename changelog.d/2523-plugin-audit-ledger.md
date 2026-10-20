### Fixed

- **plugin sandbox:** the activity log keeps each call's own time, never loses
  dropped-call counts when its ledger fills, and reads both ledgers under one
  lock. A failed DB call no longer counts as a table touched (issue #2523).
- **plugin sandbox:** rows a store returns are checked against the write-side
  ceilings. A store-assigned row id or job id the guest cannot use is refused.
  A call refused for its shape no longer spends its per-capability quota, and
  the audit target is cut before it is copied (issue #2523).
