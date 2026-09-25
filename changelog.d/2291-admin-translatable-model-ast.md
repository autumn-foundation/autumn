### Fixed

- **`generate admin` refuses a field whose model column is `#[translatable]`
  (#2291):** the refusal previously read only the admin DSL token, so
  `autumn generate admin Post title:String` was accepted against a model whose
  `title` is `#[translatable]` — and the generated admin then failed validation
  on every create and update, because `Translated` refuses the bare string the
  plain text control produces. The model AST is now the source of truth (the
  same rule `#[encrypted]` already follows, #1340): a submitted field bound to
  a `#[translatable]` model field is refused no matter how the token is
  spelled, naming the field and the attribute. The token-based refusal is
  unchanged — a `{translatable}` token on a non-translatable model is still
  refused.
