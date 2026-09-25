### Fixed

- **generate:** `autumn generate auth` now merges the `mail`, `webauthn`, and
  `webauthn-rs` Cargo features into a multi-line `features = [...]` array in
  the `[dependencies.<crate>]` subtable form. The three helpers previously
  only rewrote a single-line array and silently left the feature unset when
  the array spanned multiple lines (issue #2753).
