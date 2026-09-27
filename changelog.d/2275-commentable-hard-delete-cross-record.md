### Fixed

- **`#[commentable]`:** with `soft_delete = false`, `delete_comment` now
  refuses (`422`) a subtree that has a reply on another record, and removes
  nothing (issue #2275). Before, the `parent_id` cascade removed that reply,
  the returned count was too low, and the other record's `comment_count`
  stayed too high. The framework cannot write such a reply; imported data or
  raw SQL can.
