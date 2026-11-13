# Tabs

`autumn_web::widgets::tabs` groups related content into switchable panels —
"Profile / Security / Billing", "Overview / Comments / Activity" — with
**zero hand-written JavaScript or CSS**. Without JavaScript, switching is
pure CSS (`:target` anchors), and each panel is deep-linkable via a URL
fragment. With the shipped `autumn-widgets.js`, the tabs get APG arrow keys.

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
        (tabs("post-tabs", None, &panels))
    }
}
```

That's the whole widget: 9 lines of Maud produce a full `tablist` strip and
three panels. Visiting `/posts/42` shows the Overview panel by default;
visiting `/posts/42#comments` shows the Comments panel on load. Without
JavaScript, `:target` CSS does this.

## Signature

```rust,ignore
pub fn tabs(
    id: &str,
    active_id: Option<&str>,
    panels: &[(&str, &str, maud::Markup)],
) -> maud::Markup
```

Each tuple is `(panel_id, label, body)`, in display order. `panel_id` and
`label` are `&str` (HTML-escaped by Maud); `body` is pre-rendered
`maud::Markup` — the caller owns escaping for rich content, same as `card`'s
`body` parameter.

- `panel_id` becomes the panel's own `id`, so `#<panel_id>` in the URL
  targets it directly.
- The tab's element `id` is `"{id}-tab-{panel_id}"`.
- The `active_id` panel, else the first, is marked active (`aria-selected="true"`,
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
- **Nesting a `tabs()` widget inside another `tabs()` panel:** panel
  *visibility* handles this — the shared widget stylesheet
  (`autumn_web::ui::WIDGETS_CSS_PATH`) reveals the whole ancestor chain
  down to a deep-linked inner panel, so nested content is reachable no
  matter which outer panel it lives in. The outer widget's *active-tab
  highlight* can only sync when the shown outer panel is itself the direct
  target, though (without JavaScript) — CSS forbids nesting `:has()` inside `:has()`, so a panel
  shown only because it contains a nested target can't be tied back to a
  specific outer tab position. No outer tab is highlighted in that case,
  rather than the wrong one being highlighted.

## CSS hooks

| Selector | Element |
|---|---|
| `.autumn-tabs` | Root wrapper |
| `.autumn-tabs__list` | Tab strip (`role="tablist"`) |
| `.autumn-tabs__tab` | Individual tab link |
| `.autumn-tabs__tab--active` | The selected tab |
| `.autumn-tabs__panel` | Individual panel |
| `.autumn-tabs__panel--active` | The shown panel |
| `.autumn-tabs--enhanced` | Root, after `autumn-widgets.js` runs |

---

## Accessibility

- The tab strip carries `role="tablist"`.
- Each tab carries `role="tab"`, `aria-controls` (pointing at its panel's
  `id`), and `aria-selected`.
- Each panel carries `role="tabpanel"`, `aria-labelledby` (pointing at its
  tab's `id`), and `tabindex="0"` so it can receive keyboard focus directly.
- **With `autumn-widgets.js`:** the tab strip follows the APG tabs pattern,
  with automatic activation. `aria-selected`, the roving `tabindex` and the
  shown panel follow the selected tab, also for a deep link.
- **Without JavaScript:** `aria-selected` stays on the server's default tab.
  The server never sees the URL fragment.

## Keyboard

Load `/static/js/autumn-widgets.js` once in your layout.

| Key | Effect |
|---|---|
| ArrowRight / ArrowLeft | Select the next / previous tab. Wraps. RTL swaps them. |
| Home / End | Select the first / last tab. |
| Tab | Move focus to the shown panel. |

The URL fragment follows the selected tab, so a copied URL opens the same
tab. See [Widget Keyboard Core](keyboard.md).

## No-JavaScript switching

Each tab is a plain `<a href="#panel-id">`. The framework's shared widget
stylesheet (`autumn_web::ui::WIDGETS_CSS_PATH`) targets
`.autumn-tabs__panel:target` to show the panel matching the URL fragment,
with a `:has()`-based fallback that shows the first (`--active`) panel when
no fragment targets any panel in the widget. No `<script>`, inline event
handler, or `hx-*` attribute is emitted or required.

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
