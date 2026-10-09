### Changed

- **cms:** renaming or re-parenting a nested page no longer issues one
  query per ancestor per descendant to re-check settled paths. The guard now
  loads the subtree and its shared ancestors in batched statements (one per
  level of depth, plus one more for every further 1000 ids at a level), so
  the statement count per edit stops multiplying with subtree size (968 → 14
  for a 250-descendant page). Applies to the `examples/cms` app and the
  `autumn new --starter cms` scaffold. [no-plugin]
