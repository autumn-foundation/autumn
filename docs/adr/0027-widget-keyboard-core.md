# ADR 0027: One Delegated Keyboard Core for Widgets, Bound by `data-*`

- Status: Accepted
- Date: 2026-10-28
- Deciders: Autumn maintainers
- Tags: widgets, accessibility, javascript, csp

## Context

The widgets open and close correctly. But none of them has the WAI-ARIA APG
keyboard model inside the widget (issue #3397). Each new widget would need
the same code: arrow keys, Home/End, typeahead, Escape and click-outside.
Plugin widgets (#3400) need the same behaviour.

## Options

1. Code for each widget, as before.
2. A JS API object on `window` that widgets call.
3. One delegated core in `autumn-widgets.js`, bound by `data-*`
   attributes.

## Decision

Option 3. The core has three parts:

- `data-autumn-roving`: roving tabindex. Options: `data-autumn-orientation`,
  `data-autumn-wrap`, `data-autumn-typeahead`.
- `data-autumn-activedescendant`: focus stays on the controller. The core
  moves `aria-activedescendant`.
- `data-autumn-dismiss`: Escape, a click outside, or focus that goes
  outside closes the layer.

The controller or trigger is the element whose `aria-controls` names the
list or layer. The core dispatches `autumn:navigate` and `autumn:dismiss`.

The key listeners are on `document`. After an htmx swap, the runtime runs
widget init on the new content. That init sets the roles, the selection and
the first `tabindex`.

## Consequences

- A plugin widget gets APG keyboard behaviour from markup only.
- The server markup stays correct with JS off. JS adds the roles that need
  JS (for example `role="menu"`).
- The contract is public. A change to it is a breaking change.
- `autumn-widgets.js` has a byte budget. A unit test enforces it.
