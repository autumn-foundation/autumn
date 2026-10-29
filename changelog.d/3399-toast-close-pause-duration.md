### Added

- **widgets:** `toast_with(message, variant, &ToastConfig)` (issue #3399).
  `ToastConfig` sets the region, the `duration`, `persistent` (no timer)
  and the close label. `DEFAULT_TOAST_DURATION` is 5 s.
- **system tests:** `Page::hover(selector)` moves the mouse over an
  element, so CSS `:hover` applies.

### Changed

- **widgets:** a toast has a close button with an accessible name
  (issue #3399). `autumn-widgets.js` shows it. Closing a focused toast
  moves focus to the next toast, or back to the element that had focus
  before. Without JS, the button stays hidden.
- **widgets:** hover or focus on a toast pauses the timer of all toasts in
  its region (WCAG 2.2.1). This works without JS.
- **widgets:** `toast_region` is `popover="manual"`. The runtime opens it
  in the top layer, so a modal `<dialog>` does not cover toasts. The
  runtime also removes a toast from the DOM when its fade ends.
