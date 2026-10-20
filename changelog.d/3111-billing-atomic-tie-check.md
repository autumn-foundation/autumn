### Breaking Changes

- **Breaking:** **billing:** `store::Write` has a `Tie` variant and is
  `#[non_exhaustive]` (issue #3111,
  [migration guide](docs/migrations/next.md)). A custom `BillingStore` must
  return it from `upsert_subscription` when `SubscriptionUpsert::check_ties`
  (set with `with_tie_check`) is set.

### Fixed

- **billing:** two webhooks for one subscription, at the same second and
  with different statuses, now end at the provider's current status when they
  run at the same time. The old tie check read the row before the write, so
  both webhooks could miss it and the ranking decided. The store now reports the
  tie inside the write (issue #3111).
