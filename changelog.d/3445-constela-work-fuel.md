### Added

- **constela:** `RenderLimits::max_fuel` bounds the work of a render or a
  dispatch (issue #3445). One unit pays for a node, an expression, or 64 bytes
  that a comparison, a string index or a path lookup reads. A document that
  spends all the fuel fails with `constela.render_limit`.

### Fixed

- **constela:** a document path is now built only when a diagnostic is made. A
  long component name or key no longer costs time or memory on each node
  (issue #3445).
