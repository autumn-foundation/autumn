### Added

- **schema:** `#[renamed_from("old_name")]` on a `#[model]` field or model
  (issue #1975). `autumn schema diff` emits `ALTER TABLE ... RENAME` in place
  of a drop plus an add, so the data stays. Convention-named indexes follow
  the rename. An ambiguous hint is refused.
- **schema:** `autumn schema diff --dev-url <URL>` (issue #1975). The command
  applies the migrations to an empty dev database in one transaction, reads
  the schema back, rolls back, and diffs the models against that schema. The
  snapshot becomes optional. `AUTUMN_DEV_URL` can hold the URL.

### Fixed

- **cli:** a CLI build with `--features sqlite` compiles again. A wrapper in
  `autumn migrate` did not have the `Send` bounds that the library requires.

### Documentation

- **schema:** the declarative-schema guide now says that `schema pull` and
  doctor's database-drift check support SQLite in a `--features sqlite` build.
