### Added

- **widgets:** `popover(trigger, content, &PopoverConfig)` and
  `dropdown_menu(trigger, items, &PopoverConfig)` (issue #3402). They use
  a native `popover`, `command`/`commandfor` and CSS anchor positioning,
  with side, align and flip. `dropdown_menu` is an APG menu button with
  `role="menu"`, link and `button_to` items, separators, groups and a
  destructive variant. `autumn-widgets.js` adds the arrow keys, typeahead
  and focus return. See `docs/guide/popover-and-menu.md`.

### Breaking Changes

- **Breaking:** **widgets:** a `NavMenu` is a popover menu (issue #3402).
  With JS off, the menu starts closed, not open. The trigger has no
  `data-nav-menu-toggle`, and `.autumn-nav__menu[hidden]` is gone
  ([migration guide](docs/migrations/next.md)).

### Changed

- **widgets:** a `NavMenu` is a popover menu. The server now sends
  `role="menu"`, `role="menuitem"` and `aria-haspopup`. With JS off, the
  menu starts closed and opens with the trigger. The trigger no longer has
  `data-nav-menu-toggle`.
