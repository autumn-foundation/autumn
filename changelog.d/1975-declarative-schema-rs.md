### Added

- **schema:** `autumn schema diff --write-migration` writes `src/schema.rs`
  (issue #1975). Each managed model gets its `diesel::table!` block. The block
  of a dropped table goes, and a renamed table gets its new name, also in
  `joinable!` and `allow_tables_to_appear_in_same_query!`. Unmanaged models and
  equivalent blocks do not change. An empty plan also syncs the file.
- **schema:** two `autumn schema doctor` rows (issue #1975).
  `schema-rs-drift` finds a missing or stale `src/schema.rs` block for a
  managed model. `unmanaged-drift` compares each unmanaged model with its
  table in the live database.

### Fixed

- **schema:** the schema parser reads `#[diesel(column_name = "...")]` and a
  raw field name (`r#type`) as the SQL column name, as the `#[model]` macro
  does.

### Documentation

- **schema:** the declarative-schema guide explains how to adopt a model with
  `#[model(managed)]` and the field attributes that change its table.
