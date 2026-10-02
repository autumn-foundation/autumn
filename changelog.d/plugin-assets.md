### Added

- **assets:** plugins can ship JavaScript, CSS and fonts with content-hashed
  URLs. Declare the files once as a `PluginAssets` bundle
  (`autumn_web::plugin_assets!("my-plugin")` embeds the crate's `assets/`
  directory; `PluginAssets::from_files` takes an explicit list) and install it
  with `AppBuilder::plugin_assets`. Each file is served under
  `/static/_plugins/<namespace>/` at a hashed URL cached `immutable` for a
  year, and at its plain URL with `must-revalidate`, both with an `ETag`,
  `304` revalidation and `Range` support. `script_tag`,
  `deferred_script_tag` and `stylesheet_tag` emit the hashed URL with its
  `sha384` SRI hash, and `asset_url("_plugins/<namespace>/<file>")` resolves
  it from templates. See the [Plugin Assets guide](docs/guide/plugin-assets.md).

### Changed

- **htmx:** the framework's own scripts (`js/htmx.min.js`, `js/sse.js`,
  `js/idiomorph.min.js`, `js/autumn-htmx-csrf.js`, `js/autumn-widgets.js`)
  are now also served at content-hashed URLs, and `asset_url("js/htmx.min.js")`
  (and the others) returns the hashed URL, cached `immutable`. The plain
  `/static/js/...` paths keep working, now with an `ETag` so a revalidating
  browser gets a `304` instead of the whole script again. They are served as
  `text/javascript; charset=utf-8` rather than `application/javascript`.
- **admin:** `autumn-admin-plugin` serves `admin.js` from
  `/static/_plugins/autumn-admin/admin.<hash>.js` (with an SRI hash) instead
  of `/admin/static/admin.<hash>.js`, and its layout loads htmx and the
  widget runtime through their hashed URLs.
