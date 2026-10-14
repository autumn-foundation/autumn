### Fixed

- **cache:** the coherence audit now follows nested `dependent(...)` cascades
  (issue #2364). A root delete registers a write for every model below it, so
  a cached read of a grandchild model is flagged unless the root declares an
  edge for it.
