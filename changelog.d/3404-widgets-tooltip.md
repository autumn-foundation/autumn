### Added

- **widgets:** `tooltip` / `tooltip_with`, an accessible tooltip (issue
  #3404). The text is a `role="tooltip"` element linked by
  `aria-describedby`. CSS shows it on hover and keyboard focus, and places
  it with anchor positioning. Escape hides it through the keyboard core. A
  hover bridge keeps it open (WCAG 1.4.13). On Chromium, the runtime adds
  `interestfor` and `popover="hint"`.
- **widgets:** `data-autumn-dismiss="escape"`: a keyboard core layer that
  only Escape closes.
- **system tests:** `Page::move_mouse(x, y)` moves the pointer to a point.
- **ui:** the `--z-tooltip` token.
