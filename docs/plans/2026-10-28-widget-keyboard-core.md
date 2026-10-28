# Plan: widget keyboard core (issue #3397)

Part of #3393, Phase 0.

## Goal

Add one keyboard core to `autumn-widgets.js`. Bind it with `data-*`
attributes. Use it in tabs, `autocomplete_input` and the `NavMenu` dropdown.

## Brainstorming

Ideas we examined:

1. A delegated core: one `keydown` listener on `document`. It needs no
   re-initialisation after an htmx swap.
2. Three primitives: roving tabindex, `aria-activedescendant`, dismiss.
   Typeahead is an option of roving tabindex.
3. Find the controller from `aria-controls`. The widgets already emit it,
   so the contract needs no new id attributes.
4. A `CustomEvent` (`autumn:navigate`, `autumn:dismiss`) for each core
   action. Widgets and plugins listen for them.
5. Tabs: JS adds `autumn-tabs--enhanced`. CSS then uses `[hidden]`, not
   `:target`. The URL fragment stays in sync through `history.replaceState`.
6. Combobox: a `MutationObserver` on the listbox. It keeps `aria-expanded`
   and option ids correct after each htmx swap.
7. Menu button: JS adds `role="menu"` and `role="menuitem"`. The server
   markup stays a disclosure.
8. A `Page::press` method in the `SystemTest` harness. It sends real CDP
   key events.
9. A `Page::set_javascript_enabled` method. It lets one test prove the
   no-JS fallback.

## Reverse brainstorming

Question: "How can we make this fail?" Each answer gives a guard.

| Way to fail | Guard |
|---|---|
| Server renders `role="menu"`. With JS off, a screen reader expects arrow keys that do not work. | JS adds the menu roles. A Rust test asserts the server markup has no `role="menu"`. |
| An inline `<script>` or `on*` handler breaks `script-src 'self'`. | A browser test reads the CSP header and finds no inline script. |
| An htmx swap adds a widget that has no keyboard support. | Delegated listeners. A browser test swaps a widget in and presses keys. |
| Enter on a highlighted option submits the form. | `preventDefault` only when an option is active. A browser test checks the hidden value. |
| Home/End move the list, not the caret in the text box. | The active-descendant mode leaves Home/End to a text input. |
| `:target` CSS fights the JS state. | The `:target` rules apply only to `:not(.autumn-tabs--enhanced)`. |
| The JS grows without a limit. | A unit test fails above the documented byte budget. |
| A JS-off user loses the tabs or the menu links. | A browser test with JS off. |
| Escape closes a parent `<dialog>` as well as the menu. | `preventDefault` when the core handles Escape. |
| RTL pages invert Left and Right. | The core reads `direction` and swaps the keys. |

## Six thinking hats

- **White (facts).** Tabs have no arrow keys. `aria-selected` is fixed.
  The combobox has `aria-expanded="false"` always. The menu has only
  Escape and click-outside. The harness has no key-press method.
- **Red (feeling).** Users of a keyboard expect APG behaviour. A screen
  reader that says the wrong tab is selected causes distrust.
- **Black (risk).** APG recommends the disclosure pattern for site
  navigation. The issue asks for the menu-button pattern. We follow the
  issue, and add the roles only with JS. Changing the option `tabindex` to
  `-1` changes rendered markup. Browser tests need Chromium, so they run in
  the browser job only.
- **Yellow (value).** One core serves the tabs, listbox, menu, radio group
  and tree patterns. Plugins get a documented contract. The harness gets `press`, which #3398 can use.
- **Green (alternatives).** Manual tab activation. A JS API object on
  `window`. A dismissable layer that uses `popover`. We keep these for later
  issues (#3402, #3410).
- **Blue (process).** RED: write the Rust and browser tests first, and see
  them fail. GREEN: write the smallest JS, markup and CSS. REFACTOR: make
  nav use the shared dismiss code. Then document, review and fix.

## Scope

In scope: the core, three widgets, harness key presses, browser tests,
CSS, docs, size budget, CI step.

Out of scope: `popover` menus (#3402), combobox v2 (#3410), the axe gate
and story-wide browser runs (#3398).
