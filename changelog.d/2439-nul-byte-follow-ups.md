### Fixed

- **errors:** a NUL byte in a `JSONB` string is now a `422`, not a `500`
  (issue #2439). The classifier matches the exact Postgres message and detail
  for SQLSTATE `22P05`.
- **repository:** the generated `#[repository(api = ...)]` create and update
  handlers name the field that holds a NUL byte. The `422` now has an
  `errors[]` entry, for example `body` or `meta.key`. A payload type without
  `Serialize` keeps the empty list.

### Added

- **forms:** the NUL field error is localizable. `ChangesetForm` and
  `NestedChangesetForm` look up `common.error.nul_character` in the request
  locale when the request has an `i18n` bundle. A missing key keeps the English
  text. `autumn generate --i18n` scaffolds ship the key, and `autumn i18n check`
  does not report it unused.
- **errors:** `error::nul_byte_json_fields` returns the paths of the JSON
  strings that hold a NUL byte. `i18n::Bundle::lookup` returns a message
  without recording a miss.
