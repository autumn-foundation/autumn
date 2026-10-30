### Fixed

- **cli:** `autumn migrate check` now reads a Postgres `ALTER TABLE` action
  list correctly when a literal holds `(` or `)`. Before,
  `ADD COLUMN a text DEFAULT '(', ADD COLUMN b int NOT NULL` merged both
  actions, and the second finding was lost. It skips `'..'`, `E'..'` and
  dollar-quoted text.
