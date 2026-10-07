### Fixed

- **feed:** a date outside years 0000–9999 no longer panics an RSS feed or
  writes an invalid Atom date (issue #3093). The feed clamps the date to
  `0000-01-01T00:00:00Z` or `9999-12-31T23:59:59Z`. `Last-Modified` from
  `Feed::conditional` is clamped too.
