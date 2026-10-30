### Fixed

- **format:** `CurrencyOptions::precision(n)` no longer panics on large values
  (issue #3432). A `precision` above 28 is now treated as 28.
