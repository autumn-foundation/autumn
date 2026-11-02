### Added

- **widgets:** the small display set (issue #3406). See
  `docs/guide/display-widgets.md`.
  - `button` with `ButtonVariant` (primary, secondary, ghost, destructive,
    link), `ButtonSize`, an icon slot, icon-only, `disabled`, and two
    loading states. `.loading()` renders `aria-busy="true"`. With
    `.loading_on_request()`, the button is busy during an htmx request.
  - `button_group`: a named `role="group"` of buttons that touch.
  - `spinner`: `role="status"` with a visually hidden label. With reduced
    motion, it fades and does not turn.
  - `skeleton`: line, block or circle placeholders.
  - `.indicator(id)` on a spinner or a skeleton makes it an htmx
    `hx-indicator`. The `autumn-indicator` class hides it until the
    request runs.
  - `progress` and `meter` on the native elements, with clamped values and
    an optional visible label.
  - `separator`, `kbd`, `kbd_combo`, `empty_state` and `avatar_group`
    (`+N` overflow with a translatable name).
- **forms:** `FieldOptions::auto_grow()` and `a11y::TextArea::auto_grow()`:
  a textarea that grows with its text (`field-sizing: content`).
- **widgets:** `data-autumn-busy`: `autumn-widgets.js` sets `aria-busy` on
  the element while an htmx request runs on it or on an element around it.
