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

## Design tokens

The widget stylesheet uses the design tokens in `autumn_web::ui::tokens`
(#3396). The admin plugin, the actuator dashboard and the error pages use the
same tokens.

| Group | Tokens |
| --- | --- |
| Surface | `--bg`, `--surface`, `--surface-muted`, `--border` |
| Text | `--text`, `--text-muted` |
| Accent | `--primary`, `--primary-hover`, `--primary-light`, `--on-primary` |
| Status | `--danger*`, `--on-danger`, `--success*`, `--warning*`, `--info*` |
| Space | `--space-1` … `--space-8` (0.25rem … 2rem, scaled by density) |
| Type | `--font-size-xs` … `--font-size-2xl`, `--font-weight-*`, `--line-height-*`, `--font-family`, `--font-mono` |
| Radius | `--radius-sm`, `--radius-md`, `--radius-lg`, `--radius-full`, `--radius` |
| Z-index | `--z-dropdown`, `--z-sticky`, `--z-banner`, `--z-toast` |
| Breakpoints | `--breakpoint-sm` … `--breakpoint-xl` |

`--on-primary` and `--on-danger` are the text colors on a `--primary` or a
`--danger` fill. Do not use white text on a fill: in the dark scheme the fill
is light.

The breakpoint tokens are for reference only. `@media` cannot read `var()`.
The stylesheet uses `768px` for its one breakpoint, the same as
`--breakpoint-md`.

## Dark mode

Each color token is `light-dark(<light>, <dark>)`. The tokens set
`color-scheme: light dark` on `:root`, so the OS setting selects the scheme.
No JavaScript is necessary.

To set the scheme, put `data-theme` on `<html>`:

```html
<html data-theme="dark">   <!-- or "light"; omit it to follow the OS -->
```

`data-theme` also works on a subtree. On a subtree, also set `color` and
`background`, because an inherited color can keep the parent scheme:

```css
.promo[data-theme="dark"] { color: var(--text); background: var(--surface); }
```

A browser without `light-dark()` gets the light values.

### Let the user select the theme

1. Merge the theme route into the app:

   ```rust
   autumn_web::app()
       .merge(autumn_web::ui::theme::router())
   ```

2. Read the cookie in the layout. Render `data-theme` on `<html>`, so the first
   paint has the correct theme:

   ```rust
   use autumn_web::security::CsrfToken;
   use autumn_web::ui::theme::Theme;
   use autumn_web::widgets::{ThemeToggleConfig, theme_toggle};

   fn layout(theme: Theme, csrf: &CsrfToken, body: maud::Markup) -> maud::Markup {
       maud::html! {
           html lang="en" data-theme=[theme.data_theme()] {
               body {
                   (theme_toggle(&ThemeToggleConfig::new(theme).csrf(csrf.token(), "_csrf")))
                   (body)
               }
           }
       }
   }
   ```

   `Theme` is an extractor. Add `theme: Theme` to the handler arguments.

3. Load `autumn-widgets.js` (see the htmx guide). With the script, a click
   applies the theme at once and sends no request. Without the script, the
   form posts to `/_autumn/theme`, which stores the cookie and redirects back.

The cookie is `autumn.theme`. It holds `light` or `dark`. "System" deletes it.
It is not `HttpOnly`, because the script writes it.

If a shared cache stores pages that read the cookie, send `Vary: Cookie`.

## Density

Put `data-density` on `<html>` or on a subtree:

```html
<main data-density="compact">   <!-- compact | comfortable | spacious -->
```

Density scales the `--space-*` tokens by 0.75, 1 or 1.25. Type sizes do not
change. `autumn_web::ui::theme::Density` has the three words.

## Re-theme via tokens, not by forking the CSS

Override the tokens on `:root` in your own stylesheet. Load it after the
widget stylesheet. Use `light-dark()` to keep both schemes:

```css
:root {
    --primary: light-dark(#0369a1, #7dd3fc);
    --primary-hover: light-dark(#075985, #bae6fd);
    --on-primary: light-dark(#ffffff, #082f49);
    --radius: var(--radius-sm);
}
```

A plain value (`--primary: #0ea5e9;`) sets the token for both schemes.

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

## Class names

Every widget class has the `autumn-` prefix. Since #3396 this includes the
alert, the badge and the wizard stepper:

| Old | New |
| --- | --- |
| `alert`, `alert-info` … `alert-error` | `autumn-alert`, `autumn-alert--info` … `autumn-alert--error` |
| `alert__title`, `alert__body`, … | `autumn-alert__title`, `autumn-alert__body`, … |
| `badge`, `badge--neutral` … `badge--danger` | `autumn-badge`, `autumn-badge--neutral` … `autumn-badge--danger` |
| `wizard-progress`, `wizard-step`, `wizard-step--current`, … | `autumn-wizard-progress`, `autumn-wizard-step`, `autumn-wizard-step--current`, … |

See the [migration guide](../migrations/next.md) for the full map.

## Coverage

A build-time test (`autumn/tests/integration/widget_css_coverage.rs`) scans
every widget source file for emitted `autumn-*` classes and asserts each has a
backing rule in the shipped stylesheet — so a new widget class can't ship
unstyled without failing the build. A second test fails when a widget emits,
or the stylesheet styles, an old unprefixed class.

Contrast tests check the token pairs and the widget text in the light and the
dark scheme against WCAG AA.

## Migrating from a per-app `input.css`

If your app's `input.css` had its own `@layer components` block styling
`autumn-*` classes (the pattern `autumn new` generated previously), delete it —
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
