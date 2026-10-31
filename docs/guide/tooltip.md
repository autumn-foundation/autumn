# Tooltip

`autumn_web::widgets::tooltip` shows a short text next to a trigger. The
text describes the trigger. Hover and keyboard focus show it. Escape hides
it. It needs no JavaScript to show.

Use a tooltip for text only. For links, buttons or forms, use a popover.
Do not use `title`: keyboard users cannot see it.

A tooltip is not for touch. A touch screen has no hover, and a tap does not
always focus the trigger. Do not put the only label of a control in a
tooltip on a touch layout. Show a visible label there.

## Add a tooltip

The closure gets the id of the text. Put the id in `aria-describedby` on the
trigger:

```rust
use autumn_web::widgets::tooltip;
use maud::html;

let copy = tooltip("copy-tip", "Copy the link", |tip| html! {
    button type="button" aria-describedby=(tip) { "Copy" }
});
```

The trigger can be any focusable element: a button, a link or an input.
The id goes in the server HTML, so the link works without JavaScript.

### Icon-only trigger

An icon has no text. Put the id in `aria-labelledby`. Then the tooltip text
is the accessible name:

```rust
use autumn_web::widgets::tooltip;
use maud::html;

let delete = tooltip("delete-tip", "Delete", |tip| html! {
    button type="button" aria-labelledby=(tip) { span aria-hidden="true" { "×" } }
});
```

### Placement

```rust
use autumn_web::widgets::{TooltipConfig, TooltipPlacement, tooltip_with};
use maud::html;

let config = TooltipConfig::new().placement(TooltipPlacement::Bottom);
let docs = tooltip_with("docs-tip", "Opens the guide", &config, |tip| html! {
    a href="/docs" aria-describedby=(tip) { "Docs" }
});
```

`Top` is the default. `Start` and `End` follow the writing direction.

## Behaviour

| | CSS baseline | Chromium enhancement |
|---|---|---|
| Show | `:hover`, `:focus-visible` | `interestfor` |
| Place | anchor positioning, flips at the edge | top layer, implicit anchor |
| Hide on Escape | keyboard core (`data-autumn-dismiss="escape"`) | the runtime (`hidePopover()`) |
| Needs `autumn-widgets.js` | for Escape only | yes |

The rules (WCAG 1.4.13):

- **Hoverable.** A transparent bridge covers the gap. The pointer can move
  from the trigger onto the text.
- **Persistent.** The text has no timer. It stays until hover and focus go.
- **Dismissible.** Escape hides it. The pointer and focus do not move. The
  next hover or focus shows it again. In a modal, the first Escape hides
  the tooltip and the second closes the modal.

A mouse click does not keep the text open: it shows on `:focus-visible`,
not on each focus. A browser without `:has()` shows it on any focus.

The runtime adds `interestfor` and `popover="hint"` only on Chromium, and
only for a `<button>` or `<a href>` trigger. The server never sends them.
Firefox and Safari keep the CSS baseline. A disabled button also keeps the
CSS baseline: Chromium gives it no interest. If an htmx swap replaces the
trigger, the runtime links the new trigger.

Anchor positioning keeps the text on the screen:

- `Top` and `Bottom` flip to the other side.
- `Start` and `End` flip to the other side, then go above or below.
- `Start` and `End` follow the direction of the tooltip (`dir="rtl"`).

A hidden text is `display: none`. It adds no scroll overflow.

Without JavaScript, CSS shows and places the text. Escape does nothing.

## CSS hooks

| Selector | Element |
|---|---|
| `.autumn-tooltip` | Root `<span>`, the anchor |
| `.autumn-tooltip--top` (`--bottom`, `--start`, `--end`) | Placement |
| `.autumn-tooltip__bubble` | The `role="tooltip"` text |
| `--autumn-tooltip-gap` | Gap between the trigger and the text |
| `--z-tooltip` | Stack order without the top layer |

## Tests

`autumn/tests/integration/widgets_tooltip_browser.rs` runs each check in the
CSS mode and the Chromium mode:

```text
cargo test -p autumn-web --features system-tests \
  --test integration_tests widgets_tooltip_browser -- --include-ignored
```
