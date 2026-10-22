### Added

- **forms:** a repeated key now decodes into a `Vec<T>`, `HashSet<T>` or
  `Option<Vec<T>>` field (issue #3394). Checkbox groups, `<select multiple>`
  and tag inputs work with `ChangesetForm`, `NestedChangesetForm` and the
  multipart path. The new `autumn_web::form::from_urlencoded` applies the same
  rules to a raw body. `Changeset::field_values` returns every element of a
  sequence field for re-render. See `docs/guide/forms.md`.
- **forms:** `ChangesetForm` keeps the file parts of a `multipart/form-data`
  body: `form.file(..)`, `form.files(..)` and `form.take_files()`.
  `FormFile::new` and `ChangesetForm::with_files` build one in a test.

### Changed

- **forms:** a repeated key on a single-value field is a 422 with a message
  that names the field, for `ChangesetForm` and `NestedChangesetForm`. Before,
  it was a 400 (`duplicate field`). A `HashMap<String, String>` target also
  rejects a repeated key now. Before, the last value won.
- **forms:** a blank element of a typed list (`ids=`) is dropped, as a blank
  `Option` is. A `bool` field also accepts `on`. A blank numeric or `bool`
  `Option` is `None` even when the field has a `#[serde(default = "...")]`.
- **forms:** the `ChangesetForm` multipart path applies the `security.upload`
  policy to file parts (MIME allowlist, per-file cap). A file that fails it is
  now a 400 or a 413. Before, the decoder dropped the file with no error. The
  per-file cap also applies to each text part.
- **generate scaffold:** the bulk-delete handler decodes its ids into a
  `BulkSelection { ids: Vec<i64> }` field. The generated `parse_bulk_ids` is
  gone. A malformed selection now flashes an error and deletes nothing. Before,
  the handler dropped a non-integer id and deleted the rest.
