### Fixed

- **🗃️ Ledger: batch `examples/cms`'s single-post term lookup (statements/request
  57→23 at 12 terms, buffers 43→13):** `Repos::post_terms` (`routes/site.rs`),
  called by every public single-post view (`GET /archives/{id}`), loaded a
  post's terms with one `find_by_id` per filing — `1 + k` statements for a post
  with `k` terms. It now resolves every term in one `id = ANY(...)` lookup via
  `content::terms_by_ids`. Profiled against a ~50.5k-post fixture with posts of
  1, 4 and 12 terms: the per-term lookups drop from 1/4/12 statements to 1, and
  total statements per request become flat (23) regardless of term count.
  Rendered pages are byte-identical before and after.
