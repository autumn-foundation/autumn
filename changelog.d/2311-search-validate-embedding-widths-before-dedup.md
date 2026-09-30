### Fixed

- **search:** a batch indexing duplicate record ids validates every document's
  embedding width before deduplication picks the winner. Previously a malformed
  duplicate that lost the keep-first/keep-last coin flip was silently
  discarded, so malformed embedder output succeeded instead of surfacing
  `DimensionMismatch` (issue #2311).
