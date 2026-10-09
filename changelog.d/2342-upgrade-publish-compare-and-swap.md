### Fixed

- **upgrade:** `autumn upgrade` no longer loses a concurrent write (#2342).
  `--apply` refuses a scaffold file that changed after the preview. Two
  `--accept` commands that run together keep both pins.
