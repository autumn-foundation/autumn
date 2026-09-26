### Fixed

- **shadow:** the mirror no longer replays conditional requests to the
  candidate (issue #2335). A `GET`/`HEAD` carrying `If-None-Match`,
  `If-Modified-Since`, `If-Range`, `If-Match`, or `If-Unmodified-Since` is a
  cache revalidation whose validator is scoped to the build that issued it:
  replaying the primary's validator made the candidate answer `200` where the
  primary answered `304`, recording false `status_class` divergences on
  ordinary cache traffic — and when both builds revalidated, the differ
  compared two empty `304` bodies and recorded a vacuous `match` that masked
  genuine body regressions. Conditional requests are now skipped and counted
  as `skipped_conditional` in the `/actuator/shadow` stats, so the report
  shows how much coverage this costs on cache-heavy routes.
