### Fixed

- **🧭 Wayfinder: validate and redisplay the "Add Bookmark" form in
  `examples/bookmarks-distributed` (error-path 0/8 → 8/8, url/title/tag
  preserved):** `POST /bookmarks` never called `NewBookmark::validate()` —
  the extractor only deserialized the form, so an invalid URL or a
  blank/overlong title was silently written to Postgres and the browser was
  redirected to the list with zero feedback, rather than failing loudly.
  `create` now builds a `Changeset<NewBookmark>`, and on an invalid
  submission redisplays the same "Add Bookmark" form at `422` with every
  field preserved and an inline error next to the offending input, the same
  changeset-driven pattern the sibling `examples/bookmarks` example already
  uses for its `create`/`update` handlers.
