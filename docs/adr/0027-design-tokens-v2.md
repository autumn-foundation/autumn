# ADR 0027: Design Tokens v2 — Dark Mode, Scales, Density

- Status: Accepted
- Date: 2026-10-28
- Deciders: Autumn maintainers
- Tags: ui, css, widgets, accessibility

## Context

`autumn/src/ui/tokens.css` had 18 light-only custom properties. It had no
spacing, type, radius or z-index scale. The admin plugin kept its own copy.
The story gallery used approximately 200 lines of overrides to make a dark
preset readable. Three widget class families (`alert`, `badge`, `wizard-*`)
did not have the `autumn-` prefix (issue #3396).

## Decision

1. Each color token is `light-dark(<light>, <dark>)`. `:root` sets
   `color-scheme: light dark`, so the OS setting selects the scheme.
   `[data-theme="light|dark"]` sets `color-scheme` on any element. No JS is
   necessary.
2. A browser without `light-dark()` gets the light values from an
   `@supports not` block. A test keeps that block equal to the light values.
3. Text on a filled control uses a role token (`--on-primary`,
   `--on-danger`), not `#fff`. In dark mode the fill is light and the text is
   dark.
4. Scales: `--space-*`, `--font-size-*`, `--line-height-*`,
   `--font-weight-*`, `--radius-*`, `--z-*` and `--breakpoint-*`. The
   breakpoints are for reference only, because `@media` cannot read `var()`.
5. `[data-density="compact|comfortable|spacious"]` sets `--density-scale`
   (0.75, 1, 1.25). The space scale is declared on `:root, [data-density]`,
   so it recalculates on each element that sets a density.
6. `ui::theme::Theme` reads the `autumn.theme` cookie. The layout renders
   `data-theme` from it, so the first paint has the correct theme.
   `widgets::theme_toggle` is a form that posts to `ui::theme::THEME_PATH`.
   The app mounts `ui::theme::router()`. `autumn-widgets.js` applies the
   theme in place and writes the cookie, with no request.
7. The cookie is not `HttpOnly`, because the JS writes it. It holds one of
   three fixed words and no personal data.
8. The admin plugin embeds the core `TOKENS_CSS`.
9. `alert`, `badge` and `wizard-*` classes get the `autumn-` prefix. Variant
   classes use BEM modifiers (`autumn-alert--error`).

## Options not selected

- Mount the theme route automatically. It adds a mutating route to every
  app, which widens the security posture of each example.
- Split tokens into `--x-light` and `--x-dark`. `--primary-light` already
  names a tint, so the names would clash.
- A `GET` form. A `GET` request must not change state.

## Consequences

- Apps that target the old class names must rename their selectors. See
  `docs/migrations/next.md`.
- An app that overrides a color with a plain value loses the dark value for
  that token. It can override with its own `light-dark()`.
- A page cache shared between users must vary on `Cookie` when the layout
  reads the theme cookie.
