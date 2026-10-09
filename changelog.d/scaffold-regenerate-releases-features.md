### Fixed

- `autumn generate scaffold --force` now removes an `autumn-web` feature from `Cargo.toml` when the new scaffold does not need it. This applies to `htmx`, `multipart`, `storage`, `markdown`, `csv` and `ws`. The command keeps a feature that other code uses. It keeps `i18n` and `maud`. (#2328)
