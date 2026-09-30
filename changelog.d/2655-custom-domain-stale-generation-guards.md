### Fixed

- **custom-domains:** a stale ACME failure can no longer land on a
  re-registered generation (#2655). The issuance task now records an order's
  failure through the new `record_failure_for_registration`, which applies it
  only while the stored record is the same registration (same tenant, same
  ownership token) in an orderable state, so a late failure from a dead order
  is discarded instead of charging its reason, backoff, and alert to the
  successor — even one that is already `Verified`. `record_failure_for` also
  refuses a `PendingDns` record now, even for the owning tenant. (The verification
  half of #2655 — a stale DNS result landing on a re-registration — is
  already closed by the per-registration ownership token `apply_verification`
  guards on.)
