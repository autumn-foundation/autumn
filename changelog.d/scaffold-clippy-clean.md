### Fixed

- **generate scaffold:** the documented `autumn generate scaffold` example
  (`docs/guide/generators.md`, "Five commands to a working CRUD app") no
  longer fails a freshly scaffolded project's own generated CI (`cargo
  clippy --all-targets -- -D warnings`) on the very first push — the
  routes/main.rs it emits no longer carries an unused `serde_json` import
  (non-`--i18n` scaffolds), an unused `Update{Model}` import (non-`--live`
  scaffolds), an unused `name` parameter (`is_nullable_form_field` with no
  nullable fields), a redundant `.clone()` on a `Copy` field in the raw
  update tuple, or `Policy`/`Scope::default()` on the generated unit structs.
