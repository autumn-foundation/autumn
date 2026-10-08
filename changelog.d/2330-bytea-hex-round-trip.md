### Changed

- **generators:** a scaffolded `Bytea` column now exports, imports and edits as
  `\x` + hex (issue #2330). The old UTF-8 text lost every non-UTF-8 byte, so a
  CSV export or a form edit-and-save corrupted the column. A scaffold with
  `--import` now imports `Bytea` columns, including required ones. Index and
  show views still show the column as text. Existing scaffolds do not change.
