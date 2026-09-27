### Fixed

- **`#[commentable]`:** with `soft_delete = false`, `delete_comment` now
  refuses (`422`) a subtree that has a reply on another record, and removes
  nothing (issue #2275). Before this fix, the `parent_id` cascade removed
  that reply. The returned count was too low. The other record's
  `comment_count` stayed too high. The framework cannot write such a reply,
  but imported data or raw SQL can.
