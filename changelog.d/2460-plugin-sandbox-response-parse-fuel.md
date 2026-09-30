### Fixed

- **plugin-sandbox:** stdout writes are now fuel-charged per byte for the
  response-frame parse, not just at the bulk-copy rate (issue #2460). Every
  completed stdout line is scanned byte-by-byte by the frame parser, so a
  plugin can no longer buy a large whitespace-prefixed frame's parse at a
  memcpy's price.
