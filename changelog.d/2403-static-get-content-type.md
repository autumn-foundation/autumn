### Added

- **static:** `#[static_get]` takes `content_type = "..."` (issue #2403).
  The declared type is recorded in `manifest.json` and served as written,
  also when it is `text/plain; charset=utf-8` or `application/octet-stream`
  on a route with an asset extension. ISR does not check the handler type for
  such a route. The type applies to the page served from `dist/` only.

### Breaking Changes

- **Breaking:** `StaticRouteMeta` has a new `content_type` field. Add
  `content_type: None` to each literal
  ([migration guide](docs/migrations/next.md)).
