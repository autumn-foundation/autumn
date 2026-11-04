### Fixed

- **custom domains:** One tenant's slow DNS no longer delays other tenants'
  issuance and renewal. Verification stops after half of `poll_interval_secs`.
  A check over 20 seconds fails and backs off. A renewal DNS check over
  10 seconds is treated as a DNS error, and the renewal goes ahead
  (issue #2650).
