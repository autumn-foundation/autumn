### Breaking Changes

- **Breaking:** **widgets:** the alert, badge and wizard stepper classes have
  the `autumn-` prefix (issue #3396). `alert alert-error` is now
  `autumn-alert autumn-alert--error`, `badge badge--success` is now
  `autumn-badge autumn-badge--success`, and `wizard-*` is now
  `autumn-wizard-*` ([migration guide](docs/migrations/next.md)).

### Added

- **ui:** design tokens v2 (issue #3396). Each color token is
  `light-dark(<light>, <dark>)`, so the widgets, the admin plugin, the
  actuator dashboard and the error pages follow the OS dark mode with no JS.
  `data-theme="light|dark"` overrides the scheme on any element. New scales:
  `--space-*`, `--font-size-*`, `--font-weight-*`, `--line-height-*`,
  `--radius-*`, `--z-*` and `--breakpoint-*`. New role tokens:
  `--on-primary`, `--on-danger`, `--surface-muted`, `--info`, `--info-light`,
  `--success-text` and `--warning-text`.
- **ui:** `data-density="compact|comfortable|spacious"` scales the space
  tokens on any element.
- **widgets:** `theme_toggle` — a System / Light / Dark switch that works
  without JS. `ui::theme::Theme` reads the `autumn.theme` cookie, so the
  layout renders `data-theme` before the first paint. Merge
  `ui::theme::router()` for the no-JS `POST /_autumn/theme` route.
  `autumn-widgets.js` applies the theme in place.
- **stories:** the gallery has a theme switcher (System / Light / Dark) and a
  density switcher. They replace the four color presets.

### Changed

- **ui:** the light `--text-muted` is `#666e7b` (was `#6b7280`), so muted text
  passes WCAG AA on `--surface-muted`.
- **admin:** the plugin embeds the core tokens. Its private `tokens.css` is
  gone.
