### Added

- **widgets:** `drawer(id, title, body, &DrawerConfig)` (issue #3403). A
  native `<dialog>` on the left, right, top or bottom edge. It is modal by
  default. `DrawerConfig::modal(false)` makes a non-modal panel. It reuses
  the modal invoker command and the `closedby` fallback. Open it with
  `modal_trigger` or `drawer_trigger`.
- **widgets:** `ModalConfig::size` and `DrawerConfig::size` take a
  `DialogSize` (`Sm`, `Md`, `Lg`, `Full`).
- **widgets:** `ModalConfig::close_button(label)` and
  `DrawerConfig::close_button(label)` add a × button. `label` is its
  accessible name.
- **testing:** `Page::click_at(x, y)` sends a real mouse click at a point,
  for example on a dialog backdrop.
