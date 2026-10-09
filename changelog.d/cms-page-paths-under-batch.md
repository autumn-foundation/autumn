### Changed

- **cms:** renaming or re-parenting a nested page no longer issues one
  query per ancestor per descendant to re-check settled paths. The guard now
  loads the subtree and its shared ancestors in at most `MAX_PAGE_DEPTH + 1`
  batched statements, so statement count per edit is constant instead of
  growing with the size of the subtree (968 → 14 for a 250-descendant page).
  Applies to the `examples/cms` app and the `autumn new --starter cms`
  scaffold.
