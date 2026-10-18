### Security

- **markdown:** `ammonia` is now 4.2.4 (RUSTSEC-2026-0373). The old version
  matched the CSS `url()` function case-sensitively, so a quoted `URL("...")`
  in an allowed inline style skipped the URL scheme and domain filters.
