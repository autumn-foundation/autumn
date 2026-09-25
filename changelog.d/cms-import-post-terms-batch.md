### Fixed

- **🗃️ Ledger: batch `examples/cms`'s per-post term-reference resolution
  during import (find_by_slug calls 3000→0):** `resolve_import_terms`
  (`routes/admin/tools.rs`) called `find_by_slug` once per `(post, term)`
  reference during `POST /admin/tools/import` — a single-row `SELECT` per
  tag per post, filtered to the right taxonomy in application code
  afterward — the same N+1-on-read shape the earlier `import_terms` batching
  fix left unbatched one call up (that fix batched creating the term *rows*,
  not resolving a post's *references* to them). It now builds one
  `HashMap<(taxonomy, slug), id>` for the whole file via
  `content::resolve_term_refs` (one chunked `slug = ANY(...)` query per
  taxonomy) before the per-post loop runs, and each post does a pure
  in-memory lookup. Profiled through the real import route against a
  600-post file (5 tags each, drawn from a 60-tag vocabulary — 3,000 term
  references): `pg_stat_statements` shows the per-reference lookup drop from
  3,000 calls / 171,000 buffers to 0 calls, replaced by 2 batched `ANY()`
  calls / 114 buffers total. No behavior change: which tags land on which
  post, and an unresolvable or empty reference's handling, are verified
  identical before and after.
