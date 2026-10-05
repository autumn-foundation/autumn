### Fixed

- **billing:** the plan gate (`Entitled<R>`, `Billing::is_entitled` and
  `Billing::require`) now checks every subscription of the customer, not only
  the newest one (issue #3114). A customer with two paid plans no longer gets
  a 403 for a rule that the older plan satisfies. `require` returns the newest
  subscription that satisfies the rule. Canceled, `incomplete` and expired
  subscriptions still never grant access.
