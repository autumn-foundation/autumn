### Fixed

- `autumn generate scaffold --force` now removes an `autumn-web` feature from `Cargo.toml` when the new run no longer needs it (`htmx`, `multipart`, `storage`, `markdown`, `csv`). It keeps a feature that other code still uses. `i18n` and `maud` stay. (#2328)
