# Plugin Assets

A plugin that ships JavaScript, CSS, fonts or images compiles them into its
crate. `autumn build` never sees those files: it fingerprints the **app's**
`static/` directory, and a plugin's files are not in it. A `PluginAssets`
bundle gives them the same treatment, with no build step: the bytes are fixed
when the plugin compiles, so each file's fingerprint is computed from them.

What a bundle gets you:

- **A content-hashed URL per file** under `/static/_plugins/<namespace>/`.
  `init.js` becomes `/static/_plugins/motion/init.3f9a12c0.js`. The hash is
  the first 8 hex digits of the file's SHA-256, the same naming `autumn build`
  uses. A plugin upgrade that changes a file changes its URL.
- **Correct caching.** The hashed URL is served
  `Cache-Control: public, max-age=31536000, immutable`. The plain URL
  (`/static/_plugins/motion/init.js`) also works, with
  `public, max-age=0, must-revalidate`. Both carry an `ETag`, answer a matching
  `If-None-Match` with `304 Not Modified`, and support `Range`.
- **Subresource Integrity for free.** Each file has a `sha384` SRI hash, and
  the tag helpers put it on the `<script>`/`<link>`. There are no hash
  constants to keep in sync by hand.
- **Same-origin serving**, so the files work under Autumn's default
  `script-src 'self'` Content Security Policy.
- **Declared routes.** The bundle's routes show up in `autumn routes`,
  attributed to your plugin and classified `public`, so
  `autumn routes audit` passes.

## Ship a bundle

Put the files in a directory of the plugin crate (`assets/` by default) and
declare the bundle once, as a `static`:

```rust,ignore
use autumn_web::assets::PluginAssets;

/// Everything under `assets/`, served under `/static/_plugins/motion/`.
pub static ASSETS: PluginAssets = autumn_web::plugin_assets!("motion");
// Or name the directory:
// autumn_web::plugin_assets!("motion", "$CARGO_MANIFEST_DIR/vendor")
```

`plugin_assets!` embeds the directory with `include_dir!` and needs the
`embed-assets` feature of `autumn-web`. Dotfiles (`.gitkeep`, `.DS_Store`) are
left out. Without that feature, list the files yourself:

```rust,ignore
pub static ASSETS: PluginAssets = PluginAssets::from_files(
    "motion",
    &[
        ("motion.min.js", include_bytes!("../assets/motion.min.js")),
        ("init.js", include_bytes!("../assets/init.js")),
    ],
);
```

The namespace becomes a URL segment, so it may only contain `a-z`, `0-9`, `-`
and `_`. Anything else fails to compile.

Install the bundle from `Plugin::build`:

```rust,ignore
impl Plugin for MotionPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.plugin_assets(&ASSETS)
    }
}
```

Installing the same bundle twice (from two plugins of one family, say) is a
no-op. A *different* bundle with a namespace that is already installed panics
at startup, because the two would fight over the same URLs.

## Reference the files

The tag helpers emit the hashed URL, the SRI hash and
`crossorigin="anonymous"`:

```rust,ignore
pub fn motion_script() -> Markup {
    html! {
        (ASSETS.deferred_script_tag("motion.min.js"))
        (ASSETS.deferred_script_tag("init.js"))
    }
}

html! { head { (ASSETS.stylesheet_tag("css/theme.css")) } }
```

`script_tag` is the same without `defer`. For anything else, `ASSETS.url(path)`
returns the hashed URL and `ASSETS.get(path)` the whole file record (URLs,
SRI, bytes, content type).

An app can reference a plugin's files without importing the plugin's
`static`, through the usual `asset_url` helper:

```rust,ignore
html! { script src=(asset_url("_plugins/motion/init.js")) {} }
```

A path that names no file in the bundle logs a warning. `url`/`asset_url`
return the plain URL (which `404`s), and the tag helpers return an HTML comment
naming the missing file, so the gap shows in View Source.

## The framework's own scripts

Autumn serves its own scripts the same way: htmx, the htmx SSE extension,
idiomorph, the htmx CSRF helper and the widget runtime. Each keeps its plain
path (`/static/js/htmx.min.js`), now revalidated, and also gets a hashed one
that `asset_url` returns:

```rust,ignore
html! {
    script src=(asset_url("js/htmx.min.js")) {}
    script src=(asset_url("js/autumn-htmx-csrf.js")) {}
}
```

When an app vendors its own pinned htmx into `static/` (the `autumn assets`
commands), the built-in copy steps aside and `asset_url("js/htmx.min.js")`
resolves against the app's manifest as before.
