### Fixed

- **custom domains:** One tenant's slow DNS no longer delays other tenants'
  issuance and renewal. Verification stops after half of `poll_interval_secs`,
  and renewal DNS re-checks run concurrently with a 10-second limit
  (issue #2650).
