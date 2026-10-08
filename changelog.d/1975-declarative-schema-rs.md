### Added

- **schema:** `autumn schema diff --write-migration` writes `src/schema.rs`
  (issue #1975). Each managed model gets its `diesel::table!` block. The
  command removes the block of a dropped table and gives a renamed table its
  new name. It also updates `joinable!` and
  `allow_tables_to_appear_in_same_query!`. It does not change the block of an
  unmanaged model or a block that already matches. It also updates the file
  when the plan has no changes.
- **schema:** `autumn schema diff --write-migration` records a newly managed
  model as managed in the snapshot (issue #1975). A later delete of the model
  then drops its table.
- **schema:** two `autumn schema doctor` rows (issue #1975).
  `schema-rs-drift` finds a missing or stale `src/schema.rs` block for a
  managed model. `unmanaged-drift` compares each unmanaged model with its
  table in the live database.

### Fixed

- **schema:** the schema parser reads `#[diesel(column_name = "...")]` and a
  raw field name (`r#type`) as the SQL column name, as the `#[model]` macro
  does. If a model uses one of these, run `autumn schema snapshot` (or
  `autumn schema pull`) again to update the snapshot.

### Documentation

- **schema:** the declarative-schema guide explains how to adopt a model with
  `#[model(managed)]` and the field attributes that change its table.
