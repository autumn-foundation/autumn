### Fixed

- **cli:** `autumn destroy` removes a generated file that a newer template
  renamed or dropped (issue #2557). It deletes the file only when its
  recorded digest still matches, the templates still create a file in its
  directory, and no other Rust file names it. It keeps every other such file
  (edited, shared, a migration, recorded by Autumn 0.8.0) and names it in a
  warning. `--dry-run` lists the sweep. `.autumn/generated.toml` entries now
  carry a `scope` (`owned` or `shared`).
