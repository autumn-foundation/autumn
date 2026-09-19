### Changed

- **Macro crate split: `autumn-macros` is now four crates.**
  The database-layer codegen — `#[model]`/`#[commentable]` (`model.rs`),
  `#[repository]` (`repository.rs`), `#[service]` (`service.rs`), ~45k of the
  crate's ~87k lines — moved into the new proc-macro crates
  `autumn-macros-model` and `autumn-macros-repository`. The genuinely shared
  helpers (crate-path rewriting, schema emission, table-name inference and
  pluralisation, `unwrap_single_generic`) moved into the plain library
  `autumn-macros-support`, which each proc-macro dylib links. Core
  `autumn-macros` keeps every route/handler/edge macro unchanged, including
  `#[oauth2_callback]` (it depends on core `route`/`edge`, so it was never a
  DB-gating candidate despite an earlier report suggesting it). `autumn-web`'s
  `db` feature now enables the two new crates as optional dependencies instead
  of `autumn-macros/db` (kept as a no-op for compatibility); all public
  `autumn_web::{model, repository, service}` re-export paths are unchanged, and
  `autumn-edge` still depends only on the core macro crate. A no-database
  build never compiles the DB codegen at all.

### Fixed

- **macro crate split follow-up: `autumn_web::prelude::service` compiles
  again, and `autumn plugin list` no longer flags itself.** The split moved
  `#[service]` into `autumn-macros-model` (gated behind the `db` feature) but
  `autumn/src/prelude.rs` still re-exported it unconditionally from
  `autumn_macros`, breaking every build (`error[E0432]: unresolved import
  autumn_macros::service`); the prelude now re-exports it from
  `autumn_macros_model` under `#[cfg(feature = "db")]`, matching the
  already-correct top-level `autumn_web::service` re-export. Also: the
  `autumn-cli` plugin catalog's workspace-coverage self-test didn't know the
  three new macro crates (`autumn-macros-model`, `autumn-macros-repository`,
  `autumn-macros-support`) are core, not installable plugins; two
  `clippy::too_long_first_doc_paragraph` failures newly reachable in the
  freshly-created `autumn-macros-support` are split with a blank doc-comment
  line; and two pre-existing `-D warnings` clippy failures elsewhere in the
  workspace (a stray `Duration::from_millis(3000)` instead of
  `from_secs(3)`, a missing doc-markdown backtick around `SQLite`) are fixed
  alongside so the workspace lints clean again.
