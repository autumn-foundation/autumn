### Fixed

- **scaffold:** the CSV import's discarded-column alert now matches the decoder
  about column names (issue #2329). A file headed `title, blob` — the space
  RFC 4180 keeps — decoded its rows fine while the probe looked the row map
  up by raw name and missed the `" blob"` entry, so the "this import cannot
  set" alert never fired and the operator's edit vanished silently.
