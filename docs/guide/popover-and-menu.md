# Popover and Dropdown Menu

`popover` and `dropdown_menu` open a native `popover` element. The trigger
uses `command="toggle-popover"` and `commandfor`. CSS anchor positioning
places the panel. You write no JavaScript.

```rust
use autumn_web::prelude::*;
use http::Method;

let help = popover(
    "Help",
    &maud::html! { p { "Press ? for shortcuts." } },
    &PopoverConfig::new("help")
        .side(PopoverSide::Bottom)
        .align(PopoverAlign::Start),
);

let actions = dropdown_menu(
    "Actions",
    &[
        MenuEntry::link("Edit", "/posts/1/edit"),
        MenuEntry::group("Share", vec![MenuEntry::link("Copy link", "/posts/1/link")]),
        MenuEntry::separator(),
        MenuEntry::action("Delete", "/posts/1", Method::DELETE, csrf.token())
            .destructive(),
    ],
    &PopoverConfig::new("post-actions"),
);
```

The `id` in `PopoverConfig::new` is the id of the panel. The trigger id is
`{id}-trigger`. Each id must be unique on the page.

## What the browser does

- Click, Enter or Space on the trigger opens and closes the panel.
- A click outside, or Escape, closes the panel (light dismiss).
- When the panel closes, focus returns to the trigger.
- Opening one popover closes another.
- The panel flips to the other side when it does not fit in the viewport.
  `PopoverSide::Start` and `End` follow the writing direction.

An old browser without anchor positioning shows the panel in the middle of
the page. An old browser without invoker commands uses the
`data-popover-toggle` fallback in `autumn-widgets.js`. With JavaScript off,
such a browser cannot open the panel. The floor is a browser with invoker
commands (Baseline Dec 2025).

## Menu items

| Entry | Markup |
|---|---|
| `MenuEntry::link(label, href)` | `<a role="menuitem">` |
| `MenuEntry::action(label, action, method, csrf)` | A `button_to` form with the CSRF token. The button has `role="menuitem"`. It works with JavaScript off. Chain `.csrf_field(name)` when `security.csrf.form_field` is not `_csrf`. |
| `MenuEntry::separator()` | `<hr role="separator">` |
| `MenuEntry::group(label, entries)` | `role="group"` with a visible label |
| `.destructive()` | Adds `autumn-menu__item--destructive` to a link or form item |

The panel has `role="menu"`. The trigger has `aria-haspopup="menu"` and
`aria-controls`. The runtime sets `aria-expanded`. The server does not send
it: it cannot know the state, and a wrong value is worse than none.

## Keyboard (with `autumn-widgets.js`)

The menu is an APG menu button. The [keyboard core](keyboard.md) gives the
arrow keys, Home/End and typeahead.

| Key | Effect |
|---|---|
| Enter, Space, ArrowDown | Open the menu. Focus the first item. |
| ArrowUp | Open the menu. Focus the last item. |
| ArrowDown, ArrowUp | Next or previous item. They wrap. |
| Home, End | First or last item. |
| A letter | The next item that starts with it. |
| Space on a link item | Follow the link. |
| Escape | Close the menu. Focus returns to the trigger. |
| Tab | Close the menu. Focus goes on from the trigger. |
| Enter, or a click on an item | Do the action. Close the menu. |

Without JavaScript, the popover still opens and closes. Only the arrow keys
need JavaScript.

## `NavMenu`

A `NavMenu` in `nav_bar` is the same primitive. Its items are
`<li role="none"><a role="menuitem">`. Its trigger id is `{menu-id}-toggle`.

## CSS hooks

| Selector | Element |
|---|---|
| `.autumn-popover` | Root of `popover` and `dropdown_menu` |
| `.autumn-popover__trigger` | Trigger button |
| `.autumn-popover__panel` | The popover panel. It has `data-side` and `data-align`. |
| `.autumn-menu` | Menu panel |
| `.autumn-menu__item` | Menu item |
| `.autumn-menu__item--destructive` | Destructive item |
| `.autumn-menu__separator` | Separator |
| `.autumn-menu__group`, `.autumn-menu__label` | Group and its label |

## Tests

`autumn/tests/integration/widgets_popover_browser.rs` drives the widgets in
Chromium. Run it:

```text
cargo test -p autumn-web --features system-tests \
  --test integration_tests widgets_popover_browser -- --include-ignored
```
