### Fixed

- **billing:** dunning now ends a subscription when `invoice.payment_failed`
  arrives before the subscription event (issue #3081).
- **billing:** an invoice keeps the provider subscription id. Storing the
  subscription links its invoices and open dunning rows.
- **billing:** run the new migration
  `20261008000000_billing_invoice_provider_subscription`. Invoices stored
  before the upgrade are not linked.
- **billing:** a custom `BillingStore` must override the new
  `link_subscription` method to get the fix. The default does nothing.
