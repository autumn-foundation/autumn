### Fixed

- **cache:** `#[cached]` associated functions with the same name in one module
  no longer share cache keys (issue #2358). Add `#[cached_impl]` to the `impl`
  block, or set `#[cached(scope = "Type")]`. Scoped keys change, so the cache
  is cold after deploy.
