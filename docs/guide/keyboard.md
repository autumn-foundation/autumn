# Widget Keyboard Core

`autumn-widgets.js` has one keyboard core. It gives a widget the WAI-ARIA
APG keyboard model: arrow keys, Home/End, typeahead, Escape and
click-outside. You bind it with `data-*` attributes. You write no
JavaScript.

The core uses listeners on `document`. A widget that htmx swaps in works at
once.

The shipped widgets use the core:

| Widget | APG pattern | Keys |
|---|---|---|
| `tabs` | Tabs, automatic activation | ArrowLeft/ArrowRight, Home/End, Tab to the panel |
| `autocomplete_input` | Combobox with listbox popup | ArrowDown/ArrowUp, Enter, Escape |
| `NavMenu` in `nav_bar` | Menu button | Enter, Space, ArrowDown or ArrowUp opens; ArrowDown/ArrowUp, Home/End, a letter, Escape, Tab |

Load the runtime once in your layout:

```html
<script src="/static/js/autumn-widgets.js" defer></script>
```

## The `data-*` contract

Put the attributes on the list or the layer. Plugin widgets can use the
same attributes.

| Attribute | Effect |
|---|---|
| `data-autumn-roving[="<selector>"]` | Roving tabindex. One item is a tab stop. The arrow keys move focus. |
| `data-autumn-activedescendant` | Focus stays on the controller. The arrow keys move `aria-activedescendant`. Enter clicks the active item. |
| `data-autumn-orientation` | `horizontal` (ArrowLeft/ArrowRight), `vertical` (ArrowUp/ArrowDown) or `both` (default). |
| `data-autumn-wrap="false"` | Stop at the first and the last item. The default is to wrap. |
| `data-autumn-typeahead` | Type a letter to move to the next item that starts with it. Roving lists only. |
| `data-autumn-dismiss` | Escape, a click outside, or focus that goes outside closes the layer. |

Rules:

- **Items.** The default items are `[role=tab]`, `[role=menuitem]`,
  `[role=option]`, `[role=radio]` and `[role=treeitem]`. Give a selector to
  `data-autumn-roving` to change them. The core skips an item that has its
  own `hidden` attribute or `aria-disabled="true"`. It also skips the items
  of a nested list.
- **Controller and trigger.** The core finds them through `aria-controls`.
  The element whose `aria-controls` names the list is the controller. The
  element whose `aria-controls` names the layer is the trigger.
- **Open and close.** A layer is open when it has no `hidden` attribute and
  has at least one child element. To close it, the core sets `hidden` and
  the trigger's `aria-expanded="false"`. When focus is in the layer, Escape
  moves focus to the trigger.
- **Who opens a layer.** The core opens a layer only for ArrowDown or
  ArrowUp in an active-descendant list. In other cases your code opens it:
  remove `hidden` and set `aria-expanded="true"` on the trigger.
- **Text boxes.** With `data-autumn-activedescendant` on the list of an
  `<input>`, Home, End, ArrowLeft and ArrowRight move the caret, not the
  list.
- **Right-to-left.** In an RTL list, ArrowLeft and ArrowRight change places.
- **Modifier keys.** The core ignores a key with Ctrl, Alt or Meta, and a
  key during an IME composition.

Events (they bubble):

| Event | Target | `detail` |
|---|---|---|
| `autumn:navigate` | the list | `{ item }`, the new active item |
| `autumn:dismiss` | the layer | `{ item: null }` |

Example: a plugin listbox. Your code opens it; the core closes it.

```html
<button aria-controls="pick-list" aria-expanded="false">Pick</button>
<ul id="pick-list" role="listbox" hidden
    data-autumn-roving data-autumn-orientation="vertical"
    data-autumn-typeahead data-autumn-dismiss>
  <li role="option">Apple</li>
  <li role="option">Banana</li>
</ul>
```

## No JavaScript

Each widget works with JavaScript off:

- **Tabs** switch with `:target` CSS. Each tab is an `<a href="#panel">`.
  The runtime adds `autumn-tabs--enhanced`. Then the CSS uses `[hidden]`,
  not `:target`, and `aria-selected` follows the shown tab.
- **Autocomplete** shows its `<noscript>` `<select>`.
- **`NavMenu`** renders open, as a disclosure. The runtime adds
  `role="menu"`, `role="menuitem"` and `aria-haspopup`. These roles promise
  arrow keys, so the server does not send them.

## Content Security Policy

The runtime is a same-origin file. It uses no inline script, no `eval` and
no `on*` attribute. The default CSP `script-src 'self'` does not change.

## Size budget

| Measure | Budget |
|---|---|
| `autumn-widgets.js`, raw | 32 KiB |
| `autumn-widgets.js`, gzip level 9 | 10 KiB |

`widgets_js_stays_inside_its_size_budget` in `autumn/src/htmx.rs` fails
above the budget. Raise the budget only with a reason in the PR.

## Tests

`autumn/tests/integration/widgets_keyboard_browser.rs` sends real key
events in Chromium. It checks each pattern above, the no-JS fallback and
the CSP. Run it:

```text
cargo test -p autumn-web --features system-tests \
  --test integration_tests widgets_keyboard_browser -- --include-ignored
```

`Page::press` sends one key. `Page::set_javascript_enabled(false)` loads a
page with JavaScript off.
