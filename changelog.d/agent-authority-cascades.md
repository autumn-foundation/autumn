### Changed

- **agent authority:** a delete through a generated repository is checked
  against every model its `dependent(...)` cascade writes (issue #2451).
  `repo.delete_by_id(..)` and `repo.delete_many(..)` in an `#[agent_operable]`
  handler now need each dependent model in the grant's `writes`. `restrict`
  legs write nothing and are skipped. A `destroy` child adds its own children,
  up to 4 levels. A deeper cascade, or one that loops, is refused. Cascades
  that loop still compile.
  The same check covers the delete tool of `#[repository(.., mcp, grant = X)]`.
