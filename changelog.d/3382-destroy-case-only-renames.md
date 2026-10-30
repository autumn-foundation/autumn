### Fixed

- **destroy:** on a case-insensitive file system (macOS, Windows), `autumn
  destroy` now handles a template that changes only the case of a file or of
  its directory (issue #3382). An untouched `Foo.rs` that is now `foo.rs` is
  removed, not reported as diverged, and its manifest entry is dropped. An
  orphan in `src/widgets` is swept when the plan writes to `src/Widgets`.
