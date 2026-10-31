# Drawer and modal sizes

`drawer` is a native `<dialog>` that sits on one edge of the screen. Use it
for filters, menus and detail panels. It works without app JavaScript.

```rust
use autumn_web::prelude::*;

let config = DrawerConfig::new()
    .side(DrawerSide::Left)
    .size(DialogSize::Lg)
    .close_button("Close filters");
let body = maud::html! { p { "Filters" } };
let page = maud::html! {
    (drawer_trigger("Filters", "filters", &config, None))
    (drawer("filters", "Filters", &body, &config))
};
```

## Options

| Option | Values | Default |
|---|---|---|
| `side` | `Left`, `Right`, `Top`, `Bottom` | `Right` |
| `size` | `Sm`, `Md`, `Lg`, `Full` | `Md` |
| `modal` | `true`, `false` | `true` |
| `light_dismiss` | `true`, `false` | `true` |
| `close_button(label)` | accessible name of the × button | no button |

`ModalConfig` takes `size` and `close_button` too. `Md` adds no class, so
existing modals do not change.

## Modal and non-modal

A modal drawer opens with `showModal()`. The browser traps focus, makes the
page inert, and returns focus to the trigger on close. `drawer_trigger` uses
the same invoker command as `modal_trigger`, so no JavaScript runs.

A non-modal drawer opens with `show()`. The page stays usable. The invoker
commands have no non-modal `show`, so `drawer_trigger` emits
`data-drawer-open`, which `autumn-widgets.js` wires. Without that script, set
`DrawerConfig::open(true)` to render it open.

## Dismissal

A modal drawer closes on Escape. With `light_dismiss`, it also closes on a
click on the backdrop (`closedby="any"`). `autumn-widgets.js` adds the same
behavior in browsers that lack `closedby`.

A non-modal drawer ignores clicks on the page. With `light_dismiss`, Escape
closes it (`closedby="closerequest"`). The trigger toggles it. Pass the same
`DrawerConfig` to `drawer` and `drawer_trigger`.

## CSS hooks

`.autumn-drawer`, `--left|--right|--top|--bottom`, `--sm|--lg|--full`,
`__content`, `__title`, `__body`, `__footer`, `__close`. Set
`--autumn-drawer-size` to change the width (or the height of a top or bottom
sheet).

Modal hooks: `.autumn-modal--sm|--lg|--full`, `.autumn-modal__close`.
