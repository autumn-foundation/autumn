### Breaking Changes

- **Breaking:** **widgets:** the alert, badge and wizard stepper classes have
  the `autumn-` prefix (issue #3396). `alert alert-error` is now
  `autumn-alert autumn-alert--error`. `badge badge--success` is now
  `autumn-badge autumn-badge--success`. `wizard-*` is now `autumn-wizard-*`
  ([migration guide](docs/migrations/next.md)).

### Added

- **ui:** design tokens v2 (issue #3396). Each color token is
  `light-dark(<light>, <dark>)`, so the widgets, the admin plugin, the
  actuator dashboard and the error pages follow the OS dark mode with no JS.
  `data-theme="light|dark"` overrides the scheme on any element. New scales:
  `--space-*`, `--font-size-*`, `--font-weight-*`, `--line-height-*`,
  `--radius-*`, `--z-*` and `--breakpoint-*`. New role tokens:
  `--on-primary`, `--on-danger`, `--surface-muted`, `--info`, `--info-light`,
  `--success-text`, `--warning-text` and `--control-border` (the 3:1 border
  of a text input; the inputs use it).
- **ui:** `data-density="compact|comfortable|spacious"` scales the space
  tokens on any element.
- **widgets:** `theme_toggle` — a System / Light / Dark switch that works
  without JS. The `ui::theme::Theme` extractor reads the `autumn.theme`
  cookie. Read it in your layout and render `data-theme` on `<html>`. Merge
  `ui::theme::router()` for the no-JS `POST /_autumn/theme` route.
  `autumn-widgets.js` applies the theme in place. The admin plugin reads the
  same cookie.
- **stories:** the gallery has a theme switcher (System / Light / Dark) and a
  density switcher. They replace the four color presets.

### Changed

- **ui:** the widgets follow the OS dark mode. A page with light-only colors
  can pin the light scheme with `data-theme="light"` on `<html>`
  ([migration guide](docs/migrations/next.md)). The example apps do this.
- **ui:** the alert icon, the modal, the card and the stat card use the
  token colors, so they are correct in both schemes. The flash text uses
  the `-text` tokens and passes WCAG AA.

- **ui:** the light `--text-muted` is `#666e7b` (was `#6b7280`), so muted text
  passes WCAG AA on `--surface-muted`.
- **admin:** the plugin embeds the core tokens. Its private `tokens.css` is
  gone.
