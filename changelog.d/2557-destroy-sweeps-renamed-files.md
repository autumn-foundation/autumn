### Fixed

- **cli:** `autumn destroy` removes a generated file that a newer template
  renamed or dropped (issue #2557). It deletes the file only when its
  recorded digest still matches. It keeps an edited file, a shared file, a
  migration, and a file recorded by Autumn 0.8.0, and names each one in a
  warning. `--dry-run` lists the sweep. `.autumn/generated.toml` entries now
  carry a `scope` (`owned` or `shared`).
