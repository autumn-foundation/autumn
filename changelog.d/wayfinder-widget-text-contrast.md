### Fixed

- **ui:** status badges, alerts, the job-status success line and the upcoming
  wizard step number now meet WCAG AA text contrast (4.5:1) on the shipped
  tokens. Success and warning text is `color-mix`ed from the app's own
  `--success` / `--warning` toward `--text` (so dark-theme token overrides
  keep working, with a fixed-colour fallback for browsers without
  `color-mix`); error text uses `--danger-hover`; the upcoming wizard number
  uses `--text`. Design tokens are unchanged.
