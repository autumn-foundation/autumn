# Accordion and collapsible

`accordion` and `collapsible` build on the native `<details>` element. They
need no JavaScript. The browser gives Enter and Space on `<summary>`, focus,
and the open state.

## Accordion

```rust
use autumn_web::widgets::{AccordionConfig, accordion};
use maud::html;

let items = [
    ("Shipping", html! { p { "Ships in 2 days." } }),
    ("Returns", html! { p { "30 day returns." } }),
];
let config = AccordionConfig::new("faq").open(0);
let markup = accordion(&items, &config);
```

`AccordionConfig::new(name)` takes the accordion name. The name is the root
`id` and the shared `name` of the items. Use a different name for each
accordion on a page.

| Option | Effect |
|--------|--------|
| `multiple()` | Items open on their own. The default is exclusive: one item open at a time. |
| `open(index)` | Open an item at render. In exclusive mode, only the first open item in page order applies. An index out of range does nothing. |
| `level(HeadingLevel)` | Heading level of each title. Default `H3`. |
| `class(str)` | Extra class on the root. |
| `attrs(&[(name, value)])` | Extra attributes on the root. |

Exclusive mode uses `<details name>`. All major browsers support it. An old
browser opens items on their own: the page still works.

Each title is a heading inside `<summary>`. Screen reader users can jump
between titles by heading.

## Collapsible

```rust
use autumn_web::widgets::{CollapsibleConfig, collapsible, collapsible_with};
use maud::html;

let closed = collapsible("Advanced options", &html! { p { "…" } });
let open = collapsible_with(
    "Details",
    &html! { p { "…" } },
    &CollapsibleConfig::new().open(true),
);
```

A collapsible is one `<details>`. `attrs` go on the `<details>`. To group
collapsibles, pass `("name", "group")` in `attrs`.

## Attributes

`attrs` removes `on*` and `hx-on*` handlers, `style`, and names the widget
owns (`id`, `class`). `data-*` and `hx-*` pass through. Titles are escaped.
A body is `Markup`: escape user text in it.

## Style

Classes use the `autumn-` prefix. The stylesheet uses tokens only. The
chevron turns only when `prefers-reduced-motion: no-preference`.
