### Changed

- **stories:** the `/_stories` widget gallery now themes itself with the
  shared design tokens (`ui::tokens`) instead of hardcoded grays, and adds a
  live theme switcher (Autumn / Ocean / Forest / Midnight) — pure CSS
  `:has()`, no JavaScript, so it re-themes every previewed widget along with
  the gallery chrome. [no-plugin]: cosmetic-only change to an existing
  internal dev gallery's own chrome; no new API, widget, or agent-facing
  surface.
