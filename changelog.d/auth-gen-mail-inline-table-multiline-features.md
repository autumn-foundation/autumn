### Fixed

- **cli:** `autumn generate auth --mail` no longer panics when the target
  project's `Cargo.toml` declares `autumn-web` as an inline table
  (`autumn-web = { version = "...", features = [...] }`) whose `features`
  array spans multiple lines — it now merges `"mail"` in the same way the
  `--oauth2`/`--webauthn` flags already did (issue #2753).
