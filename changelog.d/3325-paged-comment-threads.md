### Added

- **commentable:** `comment_thread_page` reads a thread one page of
  top-level comments at a time, with every reply under them (#3325). The
  page size sets the cost. The thread size does not.
  `thread_page_containing` returns the page that shows one comment. The
  generated `{Model}Comments` trait has both.
- **widgets:** `CommentThread::page(after, next)` renders "More comments"
  and "Earlier comments" links. With htmx, "More" appends the next page in
  place.

### Changed

- **commentable:** the comment router reads one page, not the whole thread
  (#3325). Set the size with `CommentsConfig::page_size` (default 50). After
  a post, it shows the page that holds the new comment. A no-JS post returns
  to `return_to?after=<cursor>#<comment>`. See the
  [migration guide](docs/migrations/next.md).
