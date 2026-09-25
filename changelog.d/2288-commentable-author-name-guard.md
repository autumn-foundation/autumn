### Fixed

- **`#[commentable]`:** a typo'd `author_name` column (e.g.
  `author_name = usernme`) or a non-text field passed macro expansion and
  then failed at run time with an undefined-column or decoding error on the
  first request (issue #2288). The macro now reads the configured column
  through the author model's own field and binds it to the new sealed
  `autumn_web::commentable::CommentAuthorName` trait (`String` /
  `Option<String>`), so a misspelled column is a name-resolution error at
  compile time and a non-text field is rejected the same way a non-`i64`
  author key already was. The guard is emitted only when `by = <Model>`
  names an author model — an explicit `author_table` with no `by` names a
  table the macro cannot see into.
