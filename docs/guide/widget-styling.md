# Widget styling

Every framework widget — form fields, the submit button, active search,
autocomplete, the nav bar, breadcrumbs, hero banners, modals, tabs, pagination,
property lists, direct-to-storage upload, and job status — renders a semantic
`autumn-*` class (`autumn-field`, `autumn-nav`, `autumn-modal`, …), documented on
each widget's rustdoc. Historically the CSS backing those classes lived
copy-pasted in every app's Tailwind `input.css`; as of #1215 the framework ships
it itself, as one stylesheet, the same way it already ships
[flash message styling](flash.md#styling).

## Link it once

```rust
link rel="stylesheet" href=(autumn_web::ui::WIDGETS_CSS_PATH);
```

`autumn new` and the generators add this for you. It is served as a same-origin
asset (`/static/css/autumn-widgets.css`, immutably cached) — not inline styles —
so it works under a strict `style-src 'self'` Content-Security-Policy, and it is
embedded in release builds (`--embed`, #1004) with no loose files to ship.

No Tailwind build step is required to get styled widgets: the stylesheet is
plain CSS, not `@apply` rules, so linking it is enough even in a non-Tailwind
app.

## Re-theme via tokens, not by forking the CSS

Colors, borders, and radius in the widget stylesheet reference the framework's
shared design tokens (`autumn_web::ui::tokens`) — `var(--primary)`,
`var(--border)`, `var(--radius)`, etc. Re-theme by overriding the token
variables on `:root` in your own stylesheet, loaded after the widget one:

```css
:root {
    --primary: #0ea5e9;
    --primary-hover: #0284c7;
    --radius: 0.25rem;
}
```

This changes the accent color and corner radius everywhere at once — the
submit button, focus rings, the active tab underline, the current-page pager
chip — without touching a single widget rule.

## Override a specific widget

Every class is a normal, documented CSS hook — there's no ceiling. Override any
rule in your own stylesheet (loaded after the widget stylesheet) the same way
you would override a third-party component library:

```css
.autumn-submit {
    border-radius: 9999px; /* pill-shaped buttons, this app only */
}
```

## Coverage

A build-time test (`autumn/tests/integration/widget_css_coverage.rs`) scans
every widget source file for emitted `autumn-*`/`wizard-*` classes and asserts
each has a backing rule in the shipped stylesheet — so a new widget class
can't ship unstyled without failing the build.

## Migrating from a per-app `input.css` (pre-0.7)

If your app's `input.css` had its own `@layer components` block styling
`autumn-*` classes (the pattern `autumn new` generated before 0.7), delete it —
the shipped stylesheet replaces it. Two things to check afterward:

- **Accent color**: the old copy-pasted block hardcoded an indigo accent
  (`#4f46e5`) independent of `ui::tokens`. The shipped stylesheet instead
  reads `var(--primary)`, so widgets now pick up whatever `--primary` your app
  (or the framework default, violet `#7c3aed`) resolves to. Restore the old
  accent by setting `--primary: #4f46e5;` (and `--primary-hover`,
  `--primary-light`) on `:root` in your own stylesheet.
- **App-specific overrides**: if your old block also carried bespoke colors or
  layout for one widget (a themed hero banner, a differently-colored
  breadcrumb link), re-add just those declarations to your own stylesheet,
  loaded after the widget one — see "Override a specific widget" above.
