### Fixed

- **destroy:** on a case-insensitive file system (macOS, Windows), a template
  can change only the case of a file or directory name (issue #3382).
  `autumn destroy` removes an untouched `Foo.rs` when the template now names
  it `foo.rs`. It no longer reports the file as diverged, and it drops the
  manifest entry. It sweeps an orphan in `src/widgets` when the plan writes
  to `src/Widgets`.
