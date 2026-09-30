### Documentation

- **search:** `autumn-search` finally has a runnable example. `docs/guide/search.md`
  documented the plugin's full API — mounting, indexing, and the "compose
  instead of replacing" hook-composition guidance for an app that already has
  its own `MutationHooks` — entirely in `rust,ignore` snippets, with no
  example in the workspace installing the crate (issue #2320's T3 Gap 6).
  `examples/wiki` now mounts `SearchPlugin` on its existing `#[searchable]`
  `Page` model: `PageHooks` composes `enqueue_reindex_for`/`enqueue_unindex_for`
  into its existing slug/state-machine hooks rather than replacing them, and a
  new `GET /api/v1/search` route demonstrates the plugin's ranked, paginated,
  hydrated query API alongside the existing hand-rolled `/search` route. A new
  Docker-gated integration test proves the whole path — create, durable
  commit-hook, reindex job, search, delete — through the real HTTP API.
