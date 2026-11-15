### Fixed

- **wire contracts:** more read shapes keep the nested path (issue #3717).
  A typed maud binding (`@let d: &Details = &item.details`, or a typed
  `@if let`) no longer stops the path at `details`. A maud `@match` arm, a
  `match` in a macro body, a loop over a nested list (`for`, `@for`) and an
  index (`item.lines[0].price`) now read `lines.price`. So the cross-release
  check sees the removal of an optional field that these shapes read. A
  maud `@for` with a tuple pattern now shadows each name it binds.
