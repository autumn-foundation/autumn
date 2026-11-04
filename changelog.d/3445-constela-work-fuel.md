### Added

- **constela:** a render or a dispatch now has a fuel limit for its work
  (issue #3445). It is 320 units for each node that `RenderLimits::max_nodes`
  allows. One unit pays for a node, an expression, or 64 bytes that a
  comparison, a string index, a path lookup, a `markdown` source or a list
  update reads. A document that spends all the fuel fails with
  `constela.render_limit`.

### Fixed

- **constela:** a document path is now built only when a diagnostic is made. A
  long component name or key no longer costs time or memory on each node
  (issue #3445).
