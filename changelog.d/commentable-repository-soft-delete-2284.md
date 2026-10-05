### Fixed

- **`#[commentable]`:** the `{Model}Comments` helpers now use the soft-delete
  rule of the repository you call them on (issue #2284). Before, if any
  repository of the model was `soft_delete`, every helper returned `404` for a
  soft-deleted parent, also through a plain repository that returns the row.
  The generic router has no repository, so it keeps the old rule.

### Breaking Changes

- **Breaking:** the `autumn_web::commentable` functions `add_comment`,
  `comment_thread`, `delete_comment` and `recompute_comment_count` take a new
  last argument, `soft_delete: Option<bool>`. Pass `None` to keep the old
  behavior. The generated `{Model}Comments` methods do not change. See the
  [migration guide](docs/migrations/next.md#commentable-the-runtime-helpers-take-soft_delete-optionbool).
