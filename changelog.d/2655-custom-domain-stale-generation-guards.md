### Fixed

- **custom-domains:** a stale ACME failure can no longer land on a
  re-registered generation (#2655). `record_failure_for` now refuses a
  `PendingDns` record even for the owning tenant — an order never flies
  against one, so a late failure from an old order is discarded instead of
  charging its reason, backoff, and alert to the successor. (The verification
  half of #2655 — a stale DNS result landing on a re-registration — is
  already closed by the per-registration ownership token `apply_verification`
  guards on.)
