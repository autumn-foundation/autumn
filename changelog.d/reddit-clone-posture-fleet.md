### Added

- **commentable:** `commentable::route_infos(prefix)` lists the two routes
  `commentable::router()` serves. Pass it to `declare_plugin_routes` next to
  the `nest`, and `autumn routes audit` accepts the mount. Before, the nested
  router was opaque, so the audit failed on every app that mounted it. Reading
  a thread is `public`. Posting is `gated`, with `provenance: "declared"`.
- **examples:** the security posture gate now also covers `reddit-clone`. It
  is the first example in the gate with `#[secured]` routes.
- **docs:** `docs/guide/posture-gate.md` has a new recipe. It shows the
  gate's report when a `reddit-clone` route changes from `#[secured]` to
  `#[public]`. CI checks the report against golden files
  (`scripts/check-posture-demo.sh`).
