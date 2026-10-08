### Fixed

- **billing:** dunning now ends a subscription when `invoice.payment_failed`
  arrives before the subscription event (issue #3081). The invoice keeps the
  provider subscription id. Mirroring the subscription links its invoices and
  open dunning rows. Adds migration
  `20261008000000_billing_invoice_provider_subscription` and
  `BillingStore::link_subscription` (default: no-op).
