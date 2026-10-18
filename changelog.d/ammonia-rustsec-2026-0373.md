### Security

- **markdown:** `autumn-web` now needs `ammonia` 4.2.4 or later
  (RUSTSEC-2026-0373). The old version matched the CSS `url()` function
  case-sensitively, so a quoted `URL("...")` in an allowed inline style
  skipped the URL scheme and domain filters.
