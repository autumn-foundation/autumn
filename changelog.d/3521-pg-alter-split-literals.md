### Fixed

- **cli:** `autumn migrate check` no longer loses a finding when a Postgres
  `ALTER TABLE` literal holds `(` or `)`. Before,
  `ADD COLUMN a text DEFAULT '(', ADD COLUMN b int NOT NULL` merged into one
  action, and the `NOT NULL` finding was lost. The check now skips `'..'`,
  `E'..'` and dollar-quoted text.
