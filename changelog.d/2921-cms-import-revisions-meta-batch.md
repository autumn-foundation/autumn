### Fixed

- **🗃️ Ledger: batch `examples/cms`'s `import_revisions` and
  `import_post_meta` insert loops (statements 7500→300 and 5400→300):**
  `content::import_revisions` and `content::import_post_meta` are the two
  siblings the `import_terms` batching fix left unbatched — both insert
  their per-post rows one at a time inside a `for` loop, called once per
  post from the same `POST /admin/tools/import` handler. `import_revisions`
  re-writes a post's retained edit history on every import with one
  `INSERT INTO revisions` per kept revision (bounded by `REVISION_LIMIT` —
  25 — per post); `import_post_meta` restores a post's custom fields the
  same way, one `INSERT INTO post_meta` per field (unbounded per post — a
  plugin-heavy WordPress export routinely carries dozens). Both now build
  their rows first and issue one multi-row `INSERT ... VALUES (...), ...`
  per post instead. Profiled through the real import route against a
  300-post file (25 revisions and 18 custom fields each — a heavily-edited,
  plugin-instrumented blog's backup): `pg_stat_statements` shows
  `INSERT INTO revisions` calls drop 7,500→300 and the file's own
  `INSERT INTO post_meta` field-writes drop 5,400→300, with total buffers
  for both tables unchanged (same rows, same heap/index writes — the win is
  in round trips and per-statement dispatch overhead, not I/O). No behavior
  change: row counts, content and insertion order are verified identical
  before and after, and the existing `cargo test -p cms --test
  integration_test` suite (211 tests, including the revision-history and
  import/export round-trip coverage) passes unchanged.
