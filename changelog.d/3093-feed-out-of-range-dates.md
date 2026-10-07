### Fixed

- **feed:** a date outside years 0000–9999 no longer causes a panic in an RSS
  feed or an invalid date in an Atom feed (issue #3093). The feed clamps the
  date to `0000-01-01T00:00:00Z` or `9999-12-31T23:59:59Z`.
  `Feed::conditional` also clamps the `Last-Modified` header.
