# ADR 0031: Tooltip: CSS Baseline, `interestfor` as an Enhancement

- Status: Accepted
- Date: 2026-10-31
- Deciders: Autumn maintainers
- Tags: widgets, accessibility, css, javascript

## Context

Issue #3404 asks for an accessible tooltip. `title` is not accessible to
keyboard and touch users. `interestfor` and `popover="hint"` are Chromium
only, so they cannot be the baseline. The text must link to the trigger
with `aria-describedby` without JavaScript.

## Options

1. `tooltip(trigger: Markup, text)`. Add `aria-describedby` to the trigger
   HTML as text.
2. The widget renders its own `<button>`.
3. The root `<span>` gets `tabindex="0"` and the description.
4. `tooltip(id, text, |tip| trigger)`. The closure gets the id.

## Decision

Option 4. The trigger can be any element, and the link is in the server
HTML. Option 1 edits HTML as text. Option 2 is too narrow. Option 3 adds a
second tab stop.

- **Show.** CSS `:hover` and `:has(:focus-visible)` (`:focus-within`
  without `:has()`). A mouse click does not keep the text open.
- **Place.** `position: absolute`. With `anchor-scope`, anchor positioning
  with `position-try-fallbacks`. Without `anchor-scope`, all tooltips share
  one anchor name, so the gate is `anchor-scope`.
- **Escape.** The keyboard core gets `data-autumn-dismiss="escape"`: only
  Escape closes the layer. For `role="tooltip"`, the trigger is the element
  whose `aria-describedby` or `aria-labelledby` names it. The tooltip code
  closes a hover-only tooltip with the core `dismiss()`, and shows it again
  on the next hover or focus.
- **Enhance.** On Chromium, the runtime sets `popover="hint"` and
  `interestfor` on a `<button>` or `<a href>` trigger.

## Consequences

- Without JavaScript, Escape does not hide the text.
- The `autumn-widgets.js` budget goes from 36 KiB to 40 KiB raw and from
  11 KiB to 12 KiB gzip.
- Plugins can use `data-autumn-dismiss="escape"`.
