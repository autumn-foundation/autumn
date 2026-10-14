### Breaking Changes

- **Breaking:** **cache:** the coherence audit follows nested `dependent(...)`
  cascades (issue #2364, [migration guide](docs/migrations/next.md)). A root
  delete now registers a write for each model below it. A cached read of a
  grandchild model is flagged unless the root declares an edge for it, so an
  existing audit can newly fail. `MutationDescriptor` and `Mutation` have new
  public fields.
