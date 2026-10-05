### Added

- **console:** `autumn console --repl` opens an interactive Rhai prompt over
  your repositories (issue #2148). `PostRepository::find_all()`,
  `find_by_id(id)` and `count()` run with no compile per query. Rows show as
  JSON; `#[classified]`, `#[private]` and `#[serde(skip)]` fields do not show.
  `#[model]` and `#[repository]` register through `inventory`, behind the new
  off-by-default `repl` feature of autumn-web. `--repl` turns it on for the
  run only and does not change `Cargo.toml`. `SeedContext::build()` opens the
  prompt, so any playground works and its body never runs in REPL mode. The
  edit-and-run playground is unchanged. See `docs/guide/console.md`.
