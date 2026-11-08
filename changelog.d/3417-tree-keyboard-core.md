### Added

- **widgets:** the keyboard core has the APG tree keys (issue #3417). In a
  `data-autumn-roving` list with `role="tree"`, Right opens a branch or goes
  to its first child, Left closes a branch or goes to its parent, and `*`
  opens the siblings. `autumn:expand` and `autumn:collapse` fire on the
  item. A parent item holds its `role="group"`, or names it in
  `aria-owns`. Disabled tree items take focus.

### Fixed

- **widgets:** the keyboard core skips an item in a `hidden` subtree.
  ArrowDown and typeahead no longer go into a closed branch.
- **forms:** `FieldContext::group_item` does not write `required` on a
  checkbox. `required` on a box means "check this box", as
  `checkbox_group` already does.
