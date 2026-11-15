### Fixed

- **wire contracts:** more read shapes keep the nested path (issue #3717).
  A typed maud binding (`@let d: &Details = &item.details`) no longer stops
  the path at `details`. A maud `@match` arm, a `match` in a macro body, a
  loop over a nested list (`for`, `@for`) and an index now keep the nested
  path: `item.lines[0].price` reads `lines.price`. The cross-release check
  now finds a removed optional field that these shapes read, so it can
  report a break that it did not report before. A maud `@for` with a tuple
  pattern now shadows each name it binds.
