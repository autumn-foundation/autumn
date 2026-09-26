### Added

- **counter-cache / derivations:** new `parent_pk = "<column>"` override on
  `#[belongs_to(Target, counter_cache, ...)]` and `#[derivation(Target, ...)]`
  (issue #2662). Counter-cache and derivation maintenance interpolates the
  parent's primary-key column straight into `WHERE <parent>.<parent_pk> = ...`
  and always assumed `id`, so a parent whose `#[id]` field is not named `id`
  (or is renamed with `#[diesel(column_name)]`) failed at runtime on every
  insert, delete, backfill and recompute. The macro cannot see the parent's
  fields from the child, so the key is named explicitly — matching the
  existing `fk` / `parent_table` / `counter_cache_tenant` escape hatches —
  rather than inferred. Without the key the generated SQL is byte-identical
  to before.

### Fixed

- **`#[votable]` / `#[commentable]`:** the generated SQL now addresses the
  physical primary-key column when the model's `#[id]` field carries
  `#[diesel(column_name = "...")]` (issue #2662). Previously the hidden
  votable target projection, the reaction lock/update filters, and the
  commentable spec's `parent_pk` spliced the Rust field name, producing a
  nonexistent column at runtime. Unlike the child-side counter-cache /
  derivation maintenance, these macros resolve the key on the model itself,
  so no new attribute was needed — the rename is honored automatically. With
  no rename the expansion is byte-identical to before.
