### Fixed

- **ui:** status badges, alerts, the job-status success line and the upcoming
  wizard step number now meet WCAG AA text contrast (4.5:1) on the shipped
  tokens. Success and warning chip text is `color-mix`ed from the app's own
  `--success` / `--warning` toward a fixed dark (chips keep their light tint
  in dark themes); a theme with dark chips can set `--success-text` /
  `--warning-text` to override. The job-status success line mixes toward
  `--text`, so it follows dark themes. Error chip text uses `--danger-hover`;
  the upcoming wizard number uses `--text`. Design tokens are unchanged.
