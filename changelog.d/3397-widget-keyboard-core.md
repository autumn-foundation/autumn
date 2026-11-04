### Added

- **widgets:** a keyboard core in `autumn-widgets.js` (issue #3397). It
  gives roving tabindex, `aria-activedescendant`, Home/End, wrapping,
  typeahead and dismiss (Escape, click outside, focus out). Bind it with
  `data-autumn-roving`, `data-autumn-activedescendant` and
  `data-autumn-dismiss`. Plugin widgets can use the same contract. See
  `docs/guide/keyboard.md`.
- **system tests:** `Page::press(key)` sends a real key event.
  `Page::set_javascript_enabled(false)` loads a page with JavaScript off.

### Changed

- **widgets:** `tabs` has APG arrow keys with JS. `aria-selected` now
  follows the shown tab, also for a deep link. The `:target` CSS still
  switches tabs with JS off. The runtime adds `autumn-tabs--enhanced` and
  moves the `--active` classes.
- **widgets:** `autocomplete_input` has ArrowUp/ArrowDown,
  `aria-activedescendant`, Enter to select and Escape to close.
  `aria-expanded` is `"true"` only while the popup shows an option.
  `autocomplete_option` now renders `tabindex="-1"`. Focus stays on the
  input.
- **widgets:** a `NavMenu` dropdown is an APG menu button with JS. It has
  arrow keys, Home/End, typeahead, Escape and Tab to close. Its toggle
  has the id `{menu-id}-toggle`. With JS off, the menu stays an open
  list of links.
