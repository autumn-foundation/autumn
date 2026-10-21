### Fixed

- **plugin sandbox:** the activity log keeps each call's own time. It keeps the
  counts of dropped calls when its ledger is full. It reads both ledgers under
  one lock. A failed DB call no longer counts as a table touched (issue #2523).
- **plugin sandbox:** the host checks rows from a store against the write-side
  limits, and refuses a row or job id that the guest cannot use. The new denial
  `result-rejected` means the call ran: do not retry. A call refused for its
  shape no longer uses its capability quota (issue #2523).
- **plugin sandbox:** the host reserves reply room and fuel before it runs a
  call that changes state, so a committed write cannot fail its own request.
  A job carries the sha256 of its artifact. The host refuses at load a
  manifest whose call frame is larger than the stdout line limit (issue #2523).
