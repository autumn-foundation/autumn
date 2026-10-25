### Added

- **sqlite:** `autumn generate model --id uuid` now works on a SQLite app (#2555). The migration uses `TEXT PRIMARY KEY NOT NULL DEFAULT (...)` with a `randomblob` v4 UUID expression. The model field is `SqliteUuid`. The schema parser and diff accept this default.
- **sqlite:** under `--features sqlite`, `autumn_web::test::TestDb` is a temp-file SQLite database with the same API as the Postgres one. New method `TestDb::reset_table` on both backends.

### Changed

- **sqlite:** a scaffolded SQLite app now has a SQLite-native smoke test. It is not `#[ignore]`d and needs no Docker. The generator no longer warns that `cargo test` does not compile.
