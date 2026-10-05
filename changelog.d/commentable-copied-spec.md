### Fixed

- **commentable:** `add_comment`, `delete_comment`, `recompute_comment_count`
  and `comment_thread` now find the repository of the model when you give
  them a copy of a registered `CommentableSpec` (issue #2286). Before, a copy
  used only the `deleted_at` column. An audit `deleted_at` hid a live parent,
  and the helpers returned `404`. If two models register equal specs, a copy
  hides the parent when one of their repositories soft-deletes.
