### Performance

- **search: `MemorySearchBackend::vector_search` sorts only the requested
  top-`k` neighbours.** k-NN queries previously ran a full sort over the
  entire match set before truncating to `query.limit`, paying O(n log n) over
  every scored document to serve a handful of neighbours. It now reuses the
  same `select_nth_unstable_by`-based `sort_top_k` partition `keyword_search`
  already uses for its page window, cutting the ranking cost to O(n) plus a
  sort of just the `k` results returned.
