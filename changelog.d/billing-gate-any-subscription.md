### Fixed

- **billing:** the plan gate (`Entitled<R>`, `Billing::is_entitled` and
  `Billing::require`) now checks every subscription of the customer, not only
  the one that `Billing::current_subscription` shows (issue #3114). A
  customer with two paid plans no longer gets a 403 for a rule that only the
  older plan satisfies. `require` and `Entitled<R>::view` now return the
  satisfying subscription with the latest `last_event_at`. This can be an
  older row than before. `canceled`, `incomplete` and expired subscriptions
  still never grant access.
