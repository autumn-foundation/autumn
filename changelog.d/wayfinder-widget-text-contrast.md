### Fixed

- **ui:** status badges, alerts, the job-status success line and the upcoming
  wizard step number now meet WCAG AA text contrast (4.5:1) on the shipped
  tokens. `.badge--*` / `.alert-*` success, warning and error text, and
  `.autumn-job-status__success`, read `--success-text` / `--warning-text`
  (fallbacks `#065f46` / `#92400e`) or `--danger-hover`; the upcoming wizard
  number uses `--text`. Design tokens are unchanged.
