### Fixed

- **schema width:** `autumn schema width` now reads PostgreSQL `E'…'` strings
  with backslash escapes, so `E'it\'s'` no longer hides later statements. It
  also finds `table !` and `#[model (…)]` written with spaces (#3561).
