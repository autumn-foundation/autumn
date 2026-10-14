### Fixed

- **cache:** the coherence audit now follows nested `dependent(...)` cascades
  (issue #2364). A root delete now registers a write for each model below it.
  A cached read of a grandchild model is flagged unless the root declares an
  edge for it. An existing audit can newly fail. `MutationDescriptor` and
  `Mutation` gained fields, so code that builds them by hand must add them.
