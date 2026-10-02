//! Fingerprinted asset bundles compiled into a crate.
//!
//! The `autumn build` pipeline fingerprints the files in an **app's**
//! `static/` directory. A plugin crate's JavaScript, CSS, fonts and images
//! never sit in that directory: they are compiled into the plugin with
//! `include_bytes!`/`include_dir!`. A [`PluginAssets`] bundle gives those
//! files the same treatment without a build step. The bytes are fixed at
//! compile time, so each file's fingerprint comes from the bytes themselves.
//!
//! - Each file gets a content-hashed URL under `/static/_plugins/<namespace>/`
//!   (`init.js` → `/static/_plugins/motion/init.3f9a12c0.js`). The hash is
//!   the first 8 hex digits of the file's SHA-256, the same naming that
//!   `autumn build` uses for `static/`.
//! - The fingerprinted URL is served `immutable` for a year. The plain URL
//!   (`/static/_plugins/motion/init.js`) also works, with `must-revalidate`.
//!   Both carry an `ETag`, answer `If-None-Match` with `304`, and support
//!   `Range`.
//! - Each file also gets a `sha384` Subresource Integrity hash, so the
//!   `<script>`/`<link>` helpers emit `integrity=` with no hand-kept
//!   constants.
//!
//! A plugin declares the bundle once as a `static` and installs it from
//! [`Plugin::build`](crate::plugin::Plugin::build):
//!
//! ```rust,ignore
//! use autumn_web::assets::PluginAssets;
//!
//! pub static ASSETS: PluginAssets =
//!     autumn_web::plugin_assets!("motion", "$CARGO_MANIFEST_DIR/assets");
//!
//! impl Plugin for MotionPlugin {
//!     fn build(self, app: AppBuilder) -> AppBuilder {
//!         app.plugin_assets(&ASSETS)
//!     }
//! }
//!
//! // In a layout:
//! html! { head { (ASSETS.deferred_script_tag("init.js")) } }
//! ```
//!
//! Templates can also go through [`asset_url`](crate::assets::asset_url),
//! with the bundle path under `_plugins/`:
//! `asset_url("_plugins/motion/init.js")`.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// URL prefix that every plugin bundle mounts under, followed by its
/// namespace: `/static/_plugins/<namespace>/…`.
pub const PLUGIN_ASSETS_PREFIX: &str = "/static/_plugins";

/// Path of a bundle under `/static/`, without the leading `/static/`. Used
/// to recognise plugin paths in [`asset_url`](crate::assets::asset_url) and
/// the cache-control policy.
const PLUGIN_ASSETS_REL_PREFIX: &str = "_plugins/";

/// `Cache-Control` for a fingerprinted URL. Its bytes can never change.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// `Cache-Control` for a plain URL. Its bytes change when the crate does.
const REVALIDATE: &str = "public, max-age=0, must-revalidate";

/// Where a bundle's files come from.
#[derive(Clone, Copy)]
enum Source {
    /// An explicit `(logical path, bytes)` list.
    Files(&'static [(&'static str, &'static [u8])]),
    /// A directory embedded with `include_dir!`.
    #[cfg(feature = "embed-assets")]
    Dir(&'static include_dir::Dir<'static>),
}

/// Where a bundle is mounted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mount {
    /// `/static/_plugins/<namespace>/` — every plugin bundle.
    Plugin,
    /// `/static/` itself — the framework's own scripts, which keep their
    /// historical `/static/js/…` paths.
    StaticRoot,
}

/// A set of files compiled into a crate and served with content-hashed URLs.
///
/// Build one with [`plugin_assets!`](crate::plugin_assets) (a directory) or
/// [`PluginAssets::from_files`] (an explicit list), store it in a `static`,
/// and install it with
/// [`AppBuilder::plugin_assets`](crate::app::AppBuilder::plugin_assets). See
/// the [module docs](self) for the URL scheme and caching policy.
///
/// Hashes are computed once, the first time the bundle is used.
pub struct PluginAssets {
    namespace: &'static str,
    source: Source,
    mount: Mount,
    index: OnceLock<Index>,
}

impl std::fmt::Debug for PluginAssets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginAssets")
            .field("namespace", &self.namespace)
            .field("mount", &self.mount_path())
            .field("files", &self.index().assets.len())
            .finish_non_exhaustive()
    }
}

/// One file in a [`PluginAssets`] bundle, with its derived URLs and hashes.
#[derive(Debug)]
pub struct PluginAsset {
    logical_path: String,
    url: String,
    plain_url: String,
    bytes: &'static [u8],
    integrity: String,
    etag: crate::etag::ETag,
    content_type: &'static str,
}

impl PluginAsset {
    /// The path inside the bundle, e.g. `"init.js"` or `"css/theme.css"`.
    #[must_use]
    pub fn logical_path(&self) -> &str {
        &self.logical_path
    }

    /// The content-hashed URL, e.g. `/static/_plugins/motion/init.3f9a12c0.js`.
    /// Served with a year-long `immutable` cache.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The plain URL, e.g. `/static/_plugins/motion/init.js`. Served with
    /// `must-revalidate`, for references that cannot use [`url`](Self::url).
    #[must_use]
    pub fn plain_url(&self) -> &str {
        &self.plain_url
    }

    /// The `sha384-<base64>` Subresource Integrity hash of the bytes.
    #[must_use]
    pub fn integrity(&self) -> &str {
        &self.integrity
    }

    /// The file's bytes.
    #[must_use]
    pub const fn bytes(&self) -> &'static [u8] {
        self.bytes
    }

    /// The `Content-Type` the file is served with.
    #[must_use]
    pub const fn content_type(&self) -> &'static str {
        self.content_type
    }
}

/// The hashed view of a bundle, built on first use.
struct Index {
    assets: Vec<PluginAsset>,
    /// Logical path → position in `assets`.
    by_logical: HashMap<String, usize>,
    /// Fingerprinted path relative to `/static/` → position in `assets`.
    by_fingerprinted_rel: HashMap<String, usize>,
}

impl PluginAssets {
    /// A bundle from an explicit list of `(logical path, bytes)` pairs.
    ///
    /// Use this when the files are already `include_bytes!`/`include_str!`
    /// constants, or when the crate does not enable `embed-assets`:
    ///
    /// ```rust
    /// use autumn_web::assets::PluginAssets;
    ///
    /// static ASSETS: PluginAssets = PluginAssets::from_files(
    ///     "charts",
    ///     &[("charts.js", b"console.log('charts');")],
    /// );
    /// assert!(ASSETS.url("charts.js").starts_with("/static/_plugins/charts/charts."));
    /// ```
    ///
    /// A logical path is relative (no leading `/`), uses `/` separators, and
    /// has no `..` or empty segments. Entries that break these rules, and
    /// dotfiles, are skipped with a warning when the bundle is first used.
    ///
    /// # Panics
    ///
    /// Panics if `namespace` is empty, longer than 64 bytes, or contains
    /// anything other than ASCII lowercase letters, digits, `-` and `_`. In a
    /// `static` this is a compile-time error.
    #[must_use]
    pub const fn from_files(
        namespace: &'static str,
        files: &'static [(&'static str, &'static [u8])],
    ) -> Self {
        assert_valid_namespace(namespace);
        Self {
            namespace,
            source: Source::Files(files),
            mount: Mount::Plugin,
            index: OnceLock::new(),
        }
    }

    /// A bundle from a directory embedded with `include_dir!`. Every file in
    /// the tree is included, except dotfiles. Prefer the
    /// [`plugin_assets!`](crate::plugin_assets) macro, which embeds the
    /// directory for you.
    ///
    /// # Panics
    ///
    /// Same namespace rules as [`from_files`](Self::from_files).
    #[cfg(feature = "embed-assets")]
    #[must_use]
    pub const fn from_dir(
        namespace: &'static str,
        dir: &'static include_dir::Dir<'static>,
    ) -> Self {
        assert_valid_namespace(namespace);
        Self {
            namespace,
            source: Source::Dir(dir),
            mount: Mount::Plugin,
            index: OnceLock::new(),
        }
    }

    /// The framework's own scripts, mounted at `/static/` so they keep their
    /// historical paths (`/static/js/htmx.min.js`).
    #[cfg_attr(not(feature = "htmx"), allow(dead_code))]
    pub(crate) const fn framework(files: &'static [(&'static str, &'static [u8])]) -> Self {
        Self {
            namespace: "autumn",
            source: Source::Files(files),
            mount: Mount::StaticRoot,
            index: OnceLock::new(),
        }
    }

    /// The bundle's namespace, e.g. `"motion"`.
    #[must_use]
    pub const fn namespace(&self) -> &'static str {
        self.namespace
    }

    /// The URL path the bundle is served under, e.g. `/static/_plugins/motion`.
    #[must_use]
    pub fn mount_path(&self) -> String {
        match self.mount {
            Mount::Plugin => format!("{PLUGIN_ASSETS_PREFIX}/{}", self.namespace),
            Mount::StaticRoot => "/static".to_owned(),
        }
    }

    /// The file at `logical_path`, or `None` when the bundle has no such file.
    #[must_use]
    pub fn get(&self, logical_path: &str) -> Option<&PluginAsset> {
        let index = self.index();
        index
            .by_logical
            .get(logical_path)
            .map(|&i| &index.assets[i])
    }

    /// Every file in the bundle, sorted by logical path.
    pub fn iter(&self) -> impl Iterator<Item = &PluginAsset> {
        self.index().assets.iter()
    }

    /// The content-hashed URL of `logical_path`.
    ///
    /// When the bundle has no such file this logs a warning and returns the
    /// plain URL, which will `404`. Use [`get`](Self::get) to handle a
    /// missing file yourself.
    #[must_use]
    pub fn url(&self, logical_path: &str) -> String {
        if let Some(asset) = self.get(logical_path) {
            return asset.url.clone();
        }
        tracing::warn!(
            namespace = self.namespace,
            asset = logical_path,
            "PluginAssets::url: no such file in the bundle"
        );
        format!("{}/{logical_path}", self.mount_path())
    }

    /// The `sha384-<base64>` Subresource Integrity hash of `logical_path`.
    #[must_use]
    pub fn integrity(&self, logical_path: &str) -> Option<&str> {
        self.get(logical_path).map(PluginAsset::integrity)
    }

    /// A `<script>` tag for `logical_path`, with the hashed `src`, `integrity`
    /// and `crossorigin="anonymous"`.
    ///
    /// When the bundle has no such file this logs a warning and returns an
    /// HTML comment naming the file, so the gap shows in View Source.
    #[cfg(feature = "maud")]
    #[must_use]
    pub fn script_tag(&self, logical_path: &str) -> maud::Markup {
        self.get(logical_path).map_or_else(
            || self.missing_markup(logical_path),
            |asset| {
                maud::html! {
                    script src=(asset.url) integrity=(asset.integrity) crossorigin="anonymous" {}
                }
            },
        )
    }

    /// Like [`script_tag`](Self::script_tag), with the `defer` attribute.
    #[cfg(feature = "maud")]
    #[must_use]
    pub fn deferred_script_tag(&self, logical_path: &str) -> maud::Markup {
        self.get(logical_path).map_or_else(
            || self.missing_markup(logical_path),
            |asset| {
                maud::html! {
                    script src=(asset.url) integrity=(asset.integrity) crossorigin="anonymous" defer {}
                }
            },
        )
    }

    /// A `<link rel="stylesheet">` tag for `logical_path`, with the hashed
    /// `href`, `integrity` and `crossorigin="anonymous"`.
    ///
    /// A missing file behaves as in [`script_tag`](Self::script_tag).
    #[cfg(feature = "maud")]
    #[must_use]
    pub fn stylesheet_tag(&self, logical_path: &str) -> maud::Markup {
        self.get(logical_path).map_or_else(
            || self.missing_markup(logical_path),
            |asset| {
                maud::html! {
                    link rel="stylesheet" href=(asset.url) integrity=(asset.integrity) crossorigin="anonymous";
                }
            },
        )
    }

    #[cfg(feature = "maud")]
    fn missing_markup(&self, logical_path: &str) -> maud::Markup {
        tracing::warn!(
            namespace = self.namespace,
            asset = logical_path,
            "PluginAssets: no such file in the bundle"
        );
        // The path ends up inside an HTML comment. With no `>` left in it,
        // nothing in it can close the comment early.
        let safe = logical_path
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        maud::html! {
            (maud::PreEscaped(format!(
                "<!-- autumn: asset '{safe}' not found in plugin bundle '{}' -->",
                self.namespace
            )))
        }
    }

    /// A router serving every file at both its plain and its fingerprinted
    /// URL, using absolute paths. Merge it yourself only for a custom setup;
    /// [`AppBuilder::plugin_assets`](crate::app::AppBuilder::plugin_assets)
    /// mounts the bundle for you.
    pub fn router<S>(&'static self) -> axum::Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        self.router_with_base(&self.mount_path())
    }

    /// The router [`AppBuilder::plugin_assets`] nests at
    /// [`mount_path`](Self::mount_path): the same routes as
    /// [`router`](Self::router), relative to the mount.
    ///
    /// [`AppBuilder::plugin_assets`]: crate::app::AppBuilder::plugin_assets
    pub(crate) fn nested_router<S>(&'static self) -> axum::Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        self.router_with_base("")
    }

    fn router_with_base<S>(&'static self, base: &str) -> axum::Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        routes_for(self.iter(), &self.mount_path(), base)
    }

    /// The bundle's routes in the shape `autumn routes` lists them. Static
    /// files are public, so every entry is classified
    /// [`Public`](crate::route_listing::RouteClassification::Public) and
    /// does not trip `autumn routes audit`.
    #[must_use]
    pub fn route_infos(&self) -> Vec<crate::route_listing::RouteInfo> {
        self.iter()
            .flat_map(|asset| [asset.plain_url.clone(), asset.url.clone()])
            .map(|path| crate::route_listing::RouteInfo {
                method: "GET".to_owned(),
                path,
                handler: format!("{}::assets", self.namespace),
                classification: crate::route_listing::RouteClassification::Public,
                ..Default::default()
            })
            .collect()
    }

    /// `true` when `rel` (a path relative to `/static/`) is one of this
    /// bundle's fingerprinted paths.
    pub(crate) fn is_fingerprinted_rel(&self, rel: &str) -> bool {
        self.index().by_fingerprinted_rel.contains_key(rel)
    }

    fn index(&self) -> &Index {
        self.index.get_or_init(|| self.build_index())
    }

    fn build_index(&self) -> Index {
        let mut files: Vec<(String, &'static [u8])> = Vec::new();
        match self.source {
            Source::Files(list) => {
                files.extend(list.iter().map(|&(path, bytes)| (path.to_owned(), bytes)));
            }
            #[cfg(feature = "embed-assets")]
            Source::Dir(dir) => collect_dir(dir, &mut files),
        }
        files.retain(|(path, _)| {
            let valid = is_valid_logical_path(path);
            if !valid && !is_dotfile(path) {
                tracing::warn!(
                    namespace = self.namespace,
                    asset = %path,
                    "PluginAssets: skipping a file whose path is not a relative `/`-separated path"
                );
            }
            valid
        });
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files.dedup_by(|a, b| a.0 == b.0);

        let mount = self.mount_path();
        let rel_mount = mount.strip_prefix("/static").unwrap_or("");
        let rel_mount = rel_mount.strip_prefix('/').unwrap_or(rel_mount);
        let mut index = Index {
            assets: Vec::with_capacity(files.len()),
            by_logical: HashMap::with_capacity(files.len()),
            by_fingerprinted_rel: HashMap::with_capacity(files.len()),
        };
        for (logical_path, bytes) in files {
            let hashes = Hashes::of(bytes);
            let fingerprinted = fingerprinted_name(&logical_path, &hashes.short);
            let fingerprinted_rel = if rel_mount.is_empty() {
                fingerprinted.clone()
            } else {
                format!("{rel_mount}/{fingerprinted}")
            };
            let position = index.assets.len();
            index.by_logical.insert(logical_path.clone(), position);
            index
                .by_fingerprinted_rel
                .insert(fingerprinted_rel, position);
            index.assets.push(PluginAsset {
                url: format!("{mount}/{fingerprinted}"),
                plain_url: format!("{mount}/{logical_path}"),
                content_type: crate::assets::content_type_for(&logical_path),
                logical_path,
                bytes,
                integrity: hashes.integrity,
                etag: hashes.etag,
            });
        }
        index
    }
}

/// A router serving each of `assets` at its plain and its fingerprinted URL,
/// with `mount` (the bundle's mount path) replaced by `base` in each route.
pub(crate) fn routes_for<S>(
    assets: impl Iterator<Item = &'static PluginAsset>,
    mount: &str,
    base: &str,
) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let mut router = axum::Router::new();
    for asset in assets {
        for (url, immutable) in [(&asset.plain_url, false), (&asset.url, true)] {
            let relative = url.strip_prefix(mount).unwrap_or(url);
            router = router.route(
                &format!("{base}{relative}"),
                axum::routing::get(move |headers: http::HeaderMap| async move {
                    asset_response(asset, immutable, &headers)
                }),
            );
        }
    }
    router
}

/// The hashes derived from one file's bytes.
struct Hashes {
    /// First 8 hex digits of the SHA-256: the fingerprint in the filename.
    short: String,
    /// `sha384-<base64>` Subresource Integrity value.
    integrity: String,
    /// Weak `ETag` over the full SHA-256. Weak, because a compression layer
    /// may re-encode the body after the handler sets it, and a strong tag
    /// would then claim byte equality that does not hold.
    etag: crate::etag::ETag,
}

impl Hashes {
    fn of(bytes: &[u8]) -> Self {
        use base64::Engine as _;
        use sha2::{Digest as _, Sha256, Sha384};

        let sha256 = hex::encode(Sha256::digest(bytes));
        let integrity = format!(
            "sha384-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha384::digest(bytes))
        );
        Self {
            short: sha256[..8].to_owned(),
            integrity,
            etag: crate::etag::ETag::weak(sha256),
        }
    }
}

/// `dir/name.ext` → `dir/name.<hash>.ext`; `dir/name` → `dir/name.<hash>`.
/// Matches the naming `autumn build` uses for the app's `static/` tree, so a
/// `motion.min.js` becomes `motion.min.<hash>.js`.
fn fingerprinted_name(logical_path: &str, hash: &str) -> String {
    let (dir, file) = logical_path
        .rsplit_once('/')
        .map_or(("", logical_path), |(d, f)| (d, f));
    let renamed = match file.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}.{hash}.{ext}"),
        _ => format!("{file}.{hash}"),
    };
    if dir.is_empty() {
        renamed
    } else {
        format!("{dir}/{renamed}")
    }
}

/// A relative, `/`-separated path with no empty, `.`, `..` or dotfile segment.
fn is_valid_logical_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|segment| {
            !segment.is_empty() && !segment.starts_with('.') && !segment.contains('\\')
        })
}

/// `true` when any segment of `path` is a dotfile. Those are skipped quietly:
/// an embedded directory routinely holds a `.gitkeep` or `.DS_Store`.
fn is_dotfile(path: &str) -> bool {
    path.split(['/', '\\'])
        .any(|segment| segment.starts_with('.') && segment.len() > 1 && segment != "..")
}

/// Every file under `dir`, with paths relative to the embedded root.
#[cfg(feature = "embed-assets")]
fn collect_dir(dir: &'static include_dir::Dir<'static>, out: &mut Vec<(String, &'static [u8])>) {
    for entry in dir.entries() {
        match entry {
            include_dir::DirEntry::Dir(child) => collect_dir(child, out),
            include_dir::DirEntry::File(file) => {
                // `include_dir` records paths with the host's separator; URLs
                // need `/`.
                let path = file.path().to_string_lossy().replace('\\', "/");
                out.push((path, file.contents()));
            }
        }
    }
}

/// Compile-time check behind [`PluginAssets::from_files`]: the namespace
/// becomes a URL segment, so keep it to a safe, unambiguous alphabet.
const fn assert_valid_namespace(namespace: &str) {
    let bytes = namespace.as_bytes();
    assert!(
        !bytes.is_empty() && bytes.len() <= 64,
        "PluginAssets namespace must be 1 to 64 bytes long"
    );
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        assert!(
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_',
            "PluginAssets namespace may only contain a-z, 0-9, '-' and '_'"
        );
        i += 1;
    }
}

/// Serve one bundle file: conditional `GET`, `Range`, content type and the
/// cache policy for the URL form that was requested.
fn asset_response(
    asset: &'static PluginAsset,
    immutable: bool,
    request_headers: &http::HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;

    let body = bytes::Bytes::from_static(asset.bytes);
    // The tag is weak, so it can never satisfy `If-Range`; a conditional
    // range request gets the full body, as RFC 7233 requires.
    let resolution = crate::range::resolve(request_headers, body.len() as u64, None);
    let mut response = crate::range::partial_bytes_response(&resolution, body);
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static(asset.content_type),
    );
    headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static(if immutable { IMMUTABLE } else { REVALIDATE }),
    );
    crate::etag::fresh_when(request_headers, asset.etag.clone())
        .or(response)
        .into_response()
}

// ── Process-wide registry, for `asset_url` ──────────────────────────────────
//
// `asset_url` is a free function that templates call with no app handle, so
// resolving `_plugins/<ns>/…` needs a process-wide lookup. The registry only
// ever maps a namespace to a `&'static` bundle, and a bundle's URLs are a
// pure function of its compiled bytes, so two apps in one process (tests)
// registering the same bundle see identical answers. Serving stays per app:
// each `AppBuilder` mounts only the bundles it was given.

static REGISTRY: RwLock<Vec<&'static PluginAssets>> = RwLock::new(Vec::new());

/// Record `bundle` so [`asset_url`](crate::assets::asset_url) can resolve
/// its paths. Registering the same bundle again is a no-op. A different
/// bundle under a namespace that is already taken is ignored with a
/// warning; [`AppBuilder::plugin_assets`](crate::app::AppBuilder::plugin_assets)
/// refuses that case for a single app before it gets here.
pub(crate) fn register(bundle: &'static PluginAssets) {
    let mut registry = REGISTRY
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = registry
        .iter()
        .find(|existing| existing.namespace == bundle.namespace)
    {
        if !std::ptr::eq(*existing, bundle) {
            tracing::warn!(
                namespace = bundle.namespace,
                "a different PluginAssets bundle already registered this namespace in this \
                 process; asset_url keeps resolving against the first one"
            );
        }
        return;
    }
    registry.push(bundle);
}

fn registered(namespace: &str) -> Option<&'static PluginAssets> {
    REGISTRY
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|bundle| bundle.namespace == namespace)
        .copied()
}

/// Split `_plugins/<ns>/<path>` into the registered bundle and `<path>`.
fn split_plugin_rel(rel: &str) -> Option<(&'static PluginAssets, &str)> {
    let rest = rel.strip_prefix(PLUGIN_ASSETS_REL_PREFIX)?;
    let (namespace, path) = rest.split_once('/')?;
    Some((registered(namespace)?, path))
}

/// `true` when `path` (an absolute URL path) is one of the URLs a registered
/// plugin bundle serves, plain or fingerprinted.
pub(crate) fn is_registered_bundle_path(path: &str) -> bool {
    path.strip_prefix("/static/")
        .and_then(split_plugin_rel)
        .is_some_and(|(bundle, _)| {
            bundle
                .iter()
                .any(|asset| asset.plain_url == path || asset.url == path)
        })
}

/// The fingerprinted URL for `rel` (a path relative to `/static/`) when it
/// names a file in a registered plugin bundle.
pub(crate) fn resolve_registered_url(rel: &str) -> Option<String> {
    let (bundle, path) = split_plugin_rel(rel)?;
    bundle.get(path).map(|asset| asset.url.clone())
}

/// `true` when `rel` (a path relative to `/static/`) is a fingerprinted path
/// of a registered plugin bundle.
pub(crate) fn is_registered_fingerprint(rel: &str) -> bool {
    split_plugin_rel(rel).is_some_and(|(bundle, _)| bundle.is_fingerprinted_rel(rel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http::{Request, StatusCode, header};
    use tower::ServiceExt as _;

    static BUNDLE: PluginAssets = PluginAssets::from_files(
        "unit-test",
        &[
            ("init.js", b"console.log('init');"),
            ("css/theme.min.css", b"body{color:red}"),
            ("LICENSE", b"MIT"),
            (".gitkeep", b""),
            ("../escape.js", b"nope"),
            ("/abs.js", b"nope"),
        ],
    );

    fn expected_short(bytes: &[u8]) -> String {
        use sha2::{Digest as _, Sha256};
        hex::encode(&Sha256::digest(bytes)[..4])
    }

    async fn get(path: &str, headers: &[(&str, &str)]) -> axum::response::Response {
        let mut request = Request::builder().uri(path);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        BUNDLE
            .router::<()>()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[test]
    fn fingerprinted_name_matches_autumn_build_naming() {
        assert_eq!(
            fingerprinted_name("init.js", "abcd1234"),
            "init.abcd1234.js"
        );
        assert_eq!(
            fingerprinted_name("motion.min.js", "abcd1234"),
            "motion.min.abcd1234.js"
        );
        assert_eq!(
            fingerprinted_name("css/theme.css", "abcd1234"),
            "css/theme.abcd1234.css"
        );
        assert_eq!(
            fingerprinted_name("LICENSE", "abcd1234"),
            "LICENSE.abcd1234"
        );
        assert_eq!(
            fingerprinted_name("dir.v1/LICENSE", "abcd1234"),
            "dir.v1/LICENSE.abcd1234"
        );
    }

    #[test]
    fn url_is_a_function_of_the_bytes() {
        let hash = expected_short(b"console.log('init');");
        assert_eq!(
            BUNDLE.url("init.js"),
            format!("/static/_plugins/unit-test/init.{hash}.js")
        );
        let css = BUNDLE.get("css/theme.min.css").unwrap();
        assert_eq!(
            css.url(),
            format!(
                "/static/_plugins/unit-test/css/theme.min.{}.css",
                expected_short(b"body{color:red}")
            )
        );
        assert_eq!(
            css.plain_url(),
            "/static/_plugins/unit-test/css/theme.min.css"
        );
        assert_eq!(css.content_type(), "text/css; charset=utf-8");
    }

    #[test]
    fn integrity_is_sha384_of_the_bytes() {
        use base64::Engine as _;
        use sha2::{Digest as _, Sha384};
        let expected = format!(
            "sha384-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha384::digest(b"MIT"))
        );
        assert_eq!(BUNDLE.integrity("LICENSE"), Some(expected.as_str()));
    }

    #[test]
    fn unsafe_paths_and_dotfiles_are_not_part_of_the_bundle() {
        let paths: Vec<&str> = BUNDLE.iter().map(PluginAsset::logical_path).collect();
        assert_eq!(paths, ["LICENSE", "css/theme.min.css", "init.js"]);
        assert!(BUNDLE.get("../escape.js").is_none());
        assert!(BUNDLE.get("/abs.js").is_none());
        assert!(BUNDLE.get(".gitkeep").is_none());
    }

    #[test]
    fn missing_file_url_falls_back_to_the_plain_path() {
        assert_eq!(BUNDLE.url("nope.js"), "/static/_plugins/unit-test/nope.js");
        assert_eq!(BUNDLE.integrity("nope.js"), None);
    }

    #[test]
    fn route_infos_list_both_urls_as_public() {
        let infos = BUNDLE.route_infos();
        assert_eq!(infos.len(), 6, "three files, two URLs each");
        assert!(infos.iter().all(|info| info.method == "GET"
            && info.classification == crate::route_listing::RouteClassification::Public));
        assert!(infos.iter().any(|info| info.path == BUNDLE.url("init.js")));
        assert!(
            infos
                .iter()
                .any(|info| info.path == "/static/_plugins/unit-test/init.js")
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn tags_carry_hashed_url_and_integrity() {
        let asset = BUNDLE.get("init.js").unwrap();
        let script = BUNDLE.script_tag("init.js").into_string();
        assert!(
            script.contains(&format!(r#"src="{}""#, asset.url())),
            "{script}"
        );
        assert!(
            script.contains(&format!(r#"integrity="{}""#, asset.integrity())),
            "{script}"
        );
        assert!(script.contains(r#"crossorigin="anonymous""#), "{script}");
        assert!(!script.contains("defer"), "{script}");

        let deferred = BUNDLE.deferred_script_tag("init.js").into_string();
        assert!(deferred.contains("defer"), "{deferred}");

        let link = BUNDLE.stylesheet_tag("css/theme.min.css").into_string();
        assert!(link.starts_with(r#"<link rel="stylesheet""#), "{link}");
        assert!(
            link.contains(&format!(
                r#"href="{}""#,
                BUNDLE.get("css/theme.min.css").unwrap().url()
            )),
            "{link}"
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn missing_tag_is_an_html_comment_that_cannot_be_broken_out_of() {
        let markup = BUNDLE
            .script_tag("x--><script>alert(1)</script>")
            .into_string();
        assert!(markup.starts_with("<!--"), "{markup}");
        assert_eq!(markup.matches("-->").count(), 1, "{markup}");
        assert!(!markup.contains("<script"), "{markup}");
    }

    #[tokio::test]
    async fn fingerprinted_url_is_immutable() {
        let response = get(&BUNDLE.url("init.js"), &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], IMMUTABLE);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
        assert!(response.headers().contains_key(header::ETAG));
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"console.log('init');");
    }

    #[tokio::test]
    async fn plain_url_revalidates() {
        let response = get("/static/_plugins/unit-test/init.js", &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], REVALIDATE);
    }

    #[tokio::test]
    async fn matching_if_none_match_is_not_modified() {
        let first = get("/static/_plugins/unit-test/init.js", &[]).await;
        let etag = first.headers()[header::ETAG].to_str().unwrap().to_owned();
        let second = get(
            "/static/_plugins/unit-test/init.js",
            &[("if-none-match", etag.as_str())],
        )
        .await;
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(second.headers()[header::CACHE_CONTROL], REVALIDATE);
        let body = axum::body::to_bytes(second.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn range_request_is_partial() {
        let response = get(&BUNDLE.url("LICENSE"), &[("range", "bytes=0-1")]).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"MI");
    }

    #[tokio::test]
    async fn unknown_and_stale_paths_are_not_served() {
        assert_eq!(
            get("/static/_plugins/unit-test/nope.js", &[])
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get("/static/_plugins/unit-test/init.00000000.js", &[])
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get("/static/_plugins/unit-test/.gitkeep", &[])
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn registry_resolves_plugin_paths_after_registration() {
        static REGISTERED: PluginAssets =
            PluginAssets::from_files("unit-test-registry", &[("a.js", b"a")]);
        assert_eq!(
            resolve_registered_url("_plugins/unit-test-registry/a.js"),
            None
        );
        register(&REGISTERED);
        register(&REGISTERED);
        let url = REGISTERED.url("a.js");
        assert_eq!(
            resolve_registered_url("_plugins/unit-test-registry/a.js"),
            Some(url.clone())
        );
        assert_eq!(
            resolve_registered_url("_plugins/unit-test-registry/b.js"),
            None
        );
        let rel = url.strip_prefix("/static/").unwrap();
        assert!(is_registered_fingerprint(rel));
        assert!(!is_registered_fingerprint(
            "_plugins/unit-test-registry/a.js"
        ));
        assert!(!is_registered_fingerprint("js/a.js"));
        assert!(is_registered_bundle_path(&url));
        assert!(is_registered_bundle_path(
            "/static/_plugins/unit-test-registry/a.js"
        ));
        assert!(!is_registered_bundle_path(
            "/static/_plugins/unit-test-registry/b.js"
        ));
        assert!(!is_registered_bundle_path(
            "/static/_plugins/unit-test-registry"
        ));
        assert!(!is_registered_bundle_path("/static/app.js"));
    }

    #[test]
    fn a_second_bundle_cannot_take_over_a_registered_namespace() {
        static FIRST: PluginAssets =
            PluginAssets::from_files("unit-test-takeover", &[("a.js", b"first")]);
        static SECOND: PluginAssets =
            PluginAssets::from_files("unit-test-takeover", &[("a.js", b"second")]);
        register(&FIRST);
        register(&SECOND);
        assert_eq!(
            resolve_registered_url("_plugins/unit-test-takeover/a.js"),
            Some(FIRST.url("a.js"))
        );
    }

    #[test]
    fn framework_bundle_mounts_at_static_root() {
        static FRAMEWORK: PluginAssets = PluginAssets::framework(&[("js/x.js", b"x")]);
        let asset = FRAMEWORK.get("js/x.js").unwrap();
        assert_eq!(asset.plain_url(), "/static/js/x.js");
        assert_eq!(
            asset.url(),
            format!("/static/js/x.{}.js", expected_short(b"x"))
        );
        let rel = asset.url().strip_prefix("/static/").unwrap();
        assert!(FRAMEWORK.is_fingerprinted_rel(rel));
        assert!(!FRAMEWORK.is_fingerprinted_rel("js/x.js"));
    }

    #[test]
    #[should_panic(expected = "namespace may only contain")]
    fn namespace_with_a_slash_is_rejected() {
        let _ = PluginAssets::from_files("bad/ns", &[]);
    }

    #[test]
    #[should_panic(expected = "1 to 64 bytes")]
    fn empty_namespace_is_rejected() {
        let _ = PluginAssets::from_files("", &[]);
    }
}
