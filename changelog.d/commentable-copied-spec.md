### Fixed

- **commentable:** the comment helpers (`add_comment`, `delete_comment`,
  `recompute_comment_count`, `comment_thread`) now accept a copy of a
  registered `CommentableSpec` (issue #2286). Before, a copy did not find the
  repository of its model. Thus an audit `deleted_at` column hid the parent,
  and the helpers returned 404 for a live row.
