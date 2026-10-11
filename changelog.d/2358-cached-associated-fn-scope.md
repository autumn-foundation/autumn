### Fixed

- **cache:** same-named `#[cached]` associated functions in one module could
  share cache keys (issue #2358). Add `#[cached_impl]` to the `impl` block, or
  set `#[cached(scope = "Type")]`. This is opt-in. Scoped keys change, so those
  reads start with a cold cache after deploy.
