### Fixed

- **generate:** `autumn destroy` no longer leaves a shared-file edit behind
  (issue #2560). `generate` now records its shared-file edits in
  `.autumn/generated.toml`. If a project setting that the arguments do not
  show (for example `[i18n] dir`) changes, and `destroy` would not revert a
  recorded edit, `destroy` stops and names the edit. Undo the setting change,
  or pass `--force`.
