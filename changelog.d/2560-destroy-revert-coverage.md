### Fixed

- **generate:** `autumn destroy` refuses a partial destroy (issue #2560).
  `generate` now records the shared-file edits it makes in
  `.autumn/generated.toml`. If a project setting that the arguments do not show
  (for example `[i18n] dir`) changed, and `destroy` would leave one of those
  edits behind, it stops and names the edit. Undo the change, or pass `--force`.
