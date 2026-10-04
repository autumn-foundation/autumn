### Fixed

- **billing:** two subscription events with the same timestamp but different statuses (`past_due` then `active`, `active` then `paused`) no longer drop the real transition. The reconciler now asks the provider for the subscription's current state on such a tie, through the new optional `BillingProvider::fetch_subscription` (implemented for Stripe).
- **custom domains:** the ACME issuance budget now survives a restart, a deferred order no longer counts as a failure toward the failure backoff, and orders in a long pass are stamped with the time they actually ran.
