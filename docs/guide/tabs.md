# Tabs

`autumn_web::widgets::tabs` groups related content into switchable panels —
"Profile / Security / Billing", "Overview / Comments / Activity" — with
**zero hand-written JavaScript or CSS**. Switching is pure CSS (`:target`
anchors), so it works with JavaScript disabled and each panel is
deep-linkable via a URL fragment.

Out of scope: lazy loading, dynamically-added tabs, visual variants, and
related widgets like accordions or drawers.

---

## A three-tab detail view

```rust
use autumn_web::prelude::*;
use maud::html;

#[get("/posts/{id}")]
async fn show(Path(id): Path<i64>) -> Markup {
    let panels = [
        ("overview", "Overview", html! { p { "Post " (id) " overview" } }),
        ("comments", "Comments", html! { p { "Comments for post " (id) } }),
        ("activity", "Activity", html! { p { "Activity for post " (id) } }),
    ];
    html! {
        h1 { "Post " (id) }
        (tabs("post-tabs", &panels))
    }
}
```

That's the whole widget: 9 lines of Maud produce a full `tablist` strip and
three panels. Visiting `/posts/42` shows the Overview panel by default;
visiting `/posts/42#comments` shows the Comments panel on load — no
JavaScript involved either way.

## Signature

```rust,ignore
pub fn tabs(id: &str, panels: &[(&str, &str, maud::Markup)]) -> maud::Markup
```

Each tuple is `(panel_id, label, body)`, in display order. `panel_id` and
`label` are `&str` (HTML-escaped by Maud); `body` is pre-rendered
`maud::Markup` — the caller owns escaping for rich content, same as `card`'s
`body` parameter.

- `panel_id` becomes the panel's own `id`, so `#<panel_id>` in the URL
  targets it directly.
- The tab's element `id` is `"{id}-tab-{panel_id}"`.
- The first tab/panel is marked active by default (`aria-selected="true"`,
  `autumn-tabs__tab--active` / `autumn-tabs__panel--active`), so a panel is
  always visible even before any fragment is present.
- An empty `panels` slice renders the empty `autumn-tabs` container without
  panicking.
- **Rendering more than one `tabs()` widget on the same page:** panel ids
  must be unique across the *entire page*, not just within one call. Two
  `tabs()` calls that both use a panel id like `"overview"` emit two
  elements with `id="overview"` — invalid duplicate-id HTML that makes
  `:target`/`aria-controls`/`aria-labelledby` lookups ambiguous. Prefix
  panel ids per widget instance (e.g. `"post-overview"`,
  `"related-overview"`) if more than one `tabs()` appears on a page.

## CSS hooks

| Selector | Element |
|---|---|
| `.autumn-tabs` | Root wrapper |
| `.autumn-tabs__list` | Tab strip (`role="tablist"`) |
| `.autumn-tabs__tab` | Individual tab link |
| `.autumn-tabs__tab--active` | The initially-selected tab |
| `.autumn-tabs__panel` | Individual panel |
| `.autumn-tabs__panel--active` | The initially-visible panel |

---

## Accessibility

- The tab strip carries `role="tablist"`.
- Each tab carries `role="tab"`, `aria-controls` (pointing at its panel's
  `id`), and `aria-selected`.
- Each panel carries `role="tabpanel"`, `aria-labelledby` (pointing at its
  tab's `id`), and `tabindex="0"` so it can receive keyboard focus directly.
- **Known limitation:** `aria-selected` reflects the server's default
  selection (the first tab) only. The server never sees the URL fragment,
  so it can't know which tab a client-side `:target` navigation actually
  landed on — fixing this would require JavaScript, which this widget
  deliberately has none of. A screen reader always hears the first tab
  announced as "selected", even when a different panel is what's shown.

## No-JavaScript switching

Each tab is a plain `<a href="#panel-id">`. The framework's `input.css`
targets `.autumn-tabs__panel:target` to show the panel matching the URL
fragment, with a `:has()`-based fallback that shows the first (`--active`)
panel when no fragment targets any panel in the widget. No `<script>`,
inline event handler, or `hx-*` attribute is emitted or required.

The active-tab *visual highlight* also tracks whichever panel is actually
`:target`-ed, using position-based `:has()`/`:nth-child()` CSS (ids are
arbitrary caller strings, so a shared stylesheet can only match them by
position, not value) — this covers the first 6 tabs; widgets with more tabs
keep switching panels correctly, but the highlight itself stops updating
past the 6th.

`:has()` has narrower browser support than `:target`. In a browser that
supports `:target` but not `:has()`, the tabs widget degrades gracefully
rather than breaking: an `@supports not selector(:has(a))` rule
unconditionally shows the first panel, so the widget is never blank — a
fragment link to a different panel in such a browser may show that panel
*alongside* the first rather than replacing it.
