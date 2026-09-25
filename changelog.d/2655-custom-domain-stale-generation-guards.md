### Fixed

- **custom-domains:** stale ACME and verification results can no longer land
  on a re-registered generation (#2655). `record_failure_for` now refuses a
  `PendingDns` record even for the owning tenant — an order never flies
  against one, so a late failure from an old order is discarded instead of
  charging its reason, backoff, and alert to the successor — and
  `apply_verification` carries the snapshotted `tenant` +
  `registered_at_unix` from before the DNS lookup into a guarded write, so a
  stale verification neither promotes nor backs off a re-registered domain.

### Breaking Changes

- **Breaking:** `autumn_web::custom_domain::apply_verification` takes two new
  parameters — the verification's `tenant` and `registered_at_unix`,
  snapshotted before the DNS lookup — and now returns `bool`: `false` means
  the result was discarded because the hostname was re-registered while the
  lookup was in flight. See the
  [migration guide](docs/migrations/next.md#custom-domains-apply_verification-takes-a-generation-snapshot).
