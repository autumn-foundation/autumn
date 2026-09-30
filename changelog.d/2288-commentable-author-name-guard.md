### Breaking Changes

- **`#[commentable]`:** **Breaking:** with `by = <AuthorModel>`,
  `author_name` is now checked at compile time against the author struct's
  field of the same name. An author struct that renames the display-name
  column (`#[diesel(column_name = screen_name)] pub username: String`) no
  longer compiles with `author_name = screen_name` alone: add
  `author_name_field = username` to name the field (a keyword-named field is
  written raw, `author_name_field = r#type`). The field must be text: a field
  typed as a text-backed domain newtype now needs a one-line
  `impl autumn_web::commentable::CommentAuthorName for Username {}`. The SQL
  still selects the column ([migration
  guide](docs/migrations/next.md#commentable-author_name-is-checked-against-the-author-model-2288)).

### Fixed

- **`#[commentable]`:** a typo'd `author_name` column (e.g.
  `author_name = usernme`) or a non-text field passed macro expansion and
  then failed at run time with an undefined-column or decoding error on the
  first request (issue #2288). The macro now reads the configured column
  through the author model's own field and binds it to the new sealed
  `autumn_web::commentable::CommentAuthorName` trait (`String`, `Box<str>`,
  `Option<T>`, or an opted-in text newtype), so a misspelled column is a name-resolution error at
  compile time and a non-text field is rejected the same way a non-`i64`
  author key already was. The guard is emitted only when `by = <Model>`
  names an author model — an explicit `author_table` with no `by` names a
  table the macro cannot see into. The new `author_name_field` key names the
  author struct's field when it is renamed from its column.
