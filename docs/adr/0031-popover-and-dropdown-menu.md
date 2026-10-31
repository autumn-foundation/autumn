# ADR 0031: Popover and Menu on Native `popover` and Anchor Positioning

- Status: Accepted
- Date: 2026-10-31
- Deciders: Autumn maintainers
- Tags: widgets, accessibility, keyboard

## Context

Autumn had no popover. Its only dropdown was `NavMenu`, which the runtime
opened and closed with `hidden` (issue #3402). `a11y::MenuItem` had no
`role="menu"` container. Invoker commands (`command`/`commandfor`) and CSS
anchor positioning are Baseline.

## Options

1. Keep the `hidden` and `data-autumn-dismiss` model. Add JS positioning.
2. Use the native `popover` attribute, invoker commands and anchor
   positioning. Add only the APG menu keys in JS.

## Decision

Option 2.

- The browser gives open, close, light dismiss, Escape, the top layer and
  focus return. The popover works with JS off.
- The trigger is the implicit anchor, so the CSS needs no anchor names and
  no inline `style`. `data-side` and `data-align` pick `position-area`.
  `position-try-fallbacks` flips the panel.
- The server sends `role="menu"` and `role="menuitem"`. Earlier, the server
  sent no menu roles because a `NavMenu` was an open list with JS off. A
  popover is closed with JS off and still opens, so the roles are honest
  now. Only the arrow keys need JS.
- The server sends no `aria-expanded`. The runtime sets it from the `toggle`
  event.
- Menus do not use `data-autumn-dismiss`. That contract sets `hidden`, which
  fights the popover state.
- `NavMenu` moves onto the same markup. With JS off, its menus start closed,
  not open.

## Consequences

- Apps with CSS on `.autumn-nav__menu[hidden]` or on `data-nav-menu-toggle`
  must update it.
- A browser without anchor positioning shows the panel in the middle of the
  page. A browser without invoker commands uses `data-popover-toggle`.
