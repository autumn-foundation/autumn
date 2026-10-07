### Fixed

- **feed:** an out-of-range date no longer causes a panic in an RSS feed or an
  invalid date in an Atom feed (issue #3093). The feed clamps Atom dates to
  years 0000–9999. It clamps RSS dates and the `Last-Modified` header from
  `Feed::conditional` to years 1900–9999, as RFC 5322 requires.
