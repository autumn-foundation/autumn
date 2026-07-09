//! Fingerprinted asset pipeline for cache-busted static file delivery.
//!
//! In release builds, [`asset_url`] resolves a logical asset path to a
//! content-hashed URL using the manifest written by `autumn build --release`.
//! In development, it returns the plain `/static/...` URL so edits are
//! immediately visible without a build step.
//!
//! # Usage
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//!
//! html! {
//!     link rel="stylesheet" href=(asset_url("css/autumn.css"));
//!     // debug:   /static/css/autumn.css
//!     // release: /static/css/autumn.a1b2c3d4.css
//! }
//! ```
//!
//! # Manifest
//!
//! The manifest is written by `autumn build --release` to
//! `static/.autumn-manifest.json`.  It maps logical paths (relative to
//! `static/`) to fingerprinted paths:
//!
//! ```json
//! {
//!   "version": "1",
//!   "files": {
//!     "css/autumn.css": "css/autumn.a1b2c3d4.css"
//!   }
//! }
//! ```

use std::collections::HashMap;
#[cfg(feature = "embed-assets")]
use std::sync::Mutex;
use std::sync::OnceLock;

/// Filename of the fingerprint manifest within the `static/` tree.
#[cfg(any(not(debug_assertions), feature = "embed-assets"))]
const ASSET_MANIFEST_FILE: &str = ".autumn-manifest.json";

/// On-disk format of `static/.autumn-manifest.json`.
#[cfg(any(not(debug_assertions), feature = "embed-assets"))]
#[derive(Debug, serde::Deserialize)]
struct AssetManifest {
    files: HashMap<String, String>,
}

#[cfg(not(debug_assertions))]
static ASSET_MANIFEST: OnceLock<Option<AssetManifest>> = OnceLock::new();

#[cfg(not(debug_assertions))]
fn load_manifest() -> &'static Option<AssetManifest> {
    ASSET_MANIFEST.get_or_init(|| {
        let manifest_path =
            crate::app::project_dir("static", &crate::config::OsEnv).join(ASSET_MANIFEST_FILE);
        let contents = std::fs::read_to_string(manifest_path).ok()?;
        serde_json::from_str(&contents).ok()
    })
}

// ── Vendor asset manifest (.autumn-assets.json) ──────────────────────────────

/// Filename of the vendor asset manifest within the `static/` tree.
pub(crate) const VENDOR_MANIFEST_FILE: &str = ".autumn-assets.json";

/// A single vendored JS dependency recorded in the vendor manifest.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct VendorAsset {
    /// Pinned version string (e.g. `"2.0.4"`).
    pub version: String,
    /// Original download URL.
    pub source: String,
    /// Path relative to `static/` where the file is vendored (e.g. `"js/htmx.min.js"`).
    pub file: String,
    /// `sha384-<base64>` Subresource Integrity hash.
    pub integrity: String,
}

/// On-disk format of `static/.autumn-assets.json`.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct VendorManifest {
    /// Format version; currently `"1"`.
    pub version: String,
    /// Map from logical name (e.g. `"htmx"`) to asset metadata.
    pub assets: HashMap<String, VendorAsset>,
}

static VENDOR_MANIFEST: OnceLock<Option<VendorManifest>> = OnceLock::new();

fn load_vendor_manifest() -> Option<&'static VendorManifest> {
    VENDOR_MANIFEST
        .get_or_init(|| {
            // In single-binary builds the embedded dir is the sole source of truth.
            #[cfg(feature = "embed-assets")]
            if embedded_static_dir().is_some() {
                return load_embedded_vendor_manifest();
            }
            let manifest_path =
                crate::app::project_dir("static", &crate::config::OsEnv).join(VENDOR_MANIFEST_FILE);
            let contents = std::fs::read_to_string(manifest_path).ok()?;
            serde_json::from_str(&contents).ok()
        })
        .as_ref()
}

/// Returns `true` when htmx has been pinned via `autumn assets add htmx@…`.
///
/// The router uses this to skip the built-in embedded htmx handler so the
/// vendored file (and its pinned integrity hash) is served by `ServeDir` instead.
pub(crate) fn htmx_is_vendored() -> bool {
    load_vendor_manifest().is_some_and(|m| m.assets.contains_key("htmx"))
}

#[cfg(feature = "embed-assets")]
fn load_embedded_vendor_manifest() -> Option<VendorManifest> {
    EMBEDDED_STATIC
        .get()?
        .0
        .get_file(VENDOR_MANIFEST_FILE)
        .and_then(include_dir::File::contents_utf8)
        .and_then(|s| serde_json::from_str(s).ok())
}

/// Render a `<script>` tag for a vendored JS dependency.
///
/// Resolves the asset URL through [`asset_url`] (fingerprinted in release,
/// plain in dev) and emits `integrity` + `crossorigin="anonymous"` for SRI.
#[cfg(feature = "maud")]
fn render_javascript_tag(asset: &VendorAsset) -> maud::Markup {
    maud::html! {
        script
            src=(asset_url(&asset.file))
            integrity=(asset.integrity)
            crossorigin="anonymous"
            {}
    }
}

/// Render a `<script>` tag for a named vendored JS dependency.
///
/// Looks up `name` in `static/.autumn-assets.json` and emits:
/// ```html
/// <script src="{url}" integrity="{sri}" crossorigin="anonymous"></script>
/// ```
/// where `{url}` is fingerprinted in release builds and plain in dev.
///
/// When `name` is not found in the manifest the helper logs a `tracing::warn!`
/// and returns a visible HTML comment so the gap is discoverable in View Source.
/// In debug builds a `<script>console.error(…)</script>` is also emitted so the
/// error appears in the browser `DevTools` console.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::prelude::*;
///
/// html! {
///     head {
///         (javascript_include_tag("htmx"))
///     }
/// }
/// ```
#[cfg(feature = "maud")]
#[must_use]
#[allow(clippy::option_if_let_else)] // side-effecting else branch makes map_or_else harder to read
pub fn javascript_include_tag(name: &str) -> maud::Markup {
    if let Some(asset) = load_vendor_manifest().and_then(|m| m.assets.get(name)) {
        render_javascript_tag(asset)
    } else {
        tracing::warn!(
            asset = name,
            "javascript_include_tag: asset not found in {}; \
             run `autumn assets add {name}@<version>` to pin it",
            VENDOR_MANIFEST_FILE,
        );
        missing_asset_markup(name)
    }
}

/// Diagnostic markup returned when [`javascript_include_tag`] cannot find a
/// named asset.  Always contains an HTML comment visible in View Source; in
/// debug builds also fires `console.error` so `DevTools` catches the gap early.
#[cfg(feature = "maud")]
fn missing_asset_markup(name: &str) -> maud::Markup {
    let comment = format!(
        "<!-- autumn: asset '{name}' not found in {VENDOR_MANIFEST_FILE}; \
         run `autumn assets add {name}@<version>` -->"
    );
    if cfg!(debug_assertions) {
        let console_err = format!(
            "console.error('[autumn] asset not found: {name} \
             \u{2014} run autumn assets add {name}@<version>');"
        );
        maud::html! {
            (maud::PreEscaped(comment))
            script { (maud::PreEscaped(console_err)) }
        }
    } else {
        maud::html! { (maud::PreEscaped(comment)) }
    }
}

// ── Embedded assets (feature = "embed-assets") ──────────────────────────────
//
// When the app registers an embedded `static/` tree via
// [`AppBuilder::embedded_static`](crate::app::AppBuilder::embedded_static),
// the binary is fully self-contained: `asset_url`/`is_manifest_asset` resolve
// against the manifest baked into the binary (no disk read) and `/static/*`
// is served from the embedded bytes. Both the manifest and the files come from
// the *same* build, so fingerprint-vs-manifest drift is impossible.
//
// The embedded path is active regardless of `debug_assertions`: it engages only
// once a dir is registered, so dev builds (which never register one) are
// unaffected and keep serving from disk for hot-reload.

/// A `static/` directory embedded into the binary at compile time.
///
/// Produced by [`embed_static!`](crate::embed_static) and handed to
/// [`AppBuilder::embedded_static`](crate::app::AppBuilder::embedded_static).
#[cfg(feature = "embed-assets")]
#[derive(Clone, Copy)]
pub struct EmbeddedStaticDir(pub &'static include_dir::Dir<'static>);

#[cfg(feature = "embed-assets")]
impl std::fmt::Debug for EmbeddedStaticDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedStaticDir")
            .field("path", &self.0.path())
            .finish()
    }
}

#[cfg(feature = "embed-assets")]
static EMBEDDED_STATIC: OnceLock<EmbeddedStaticDir> = OnceLock::new();

#[cfg(feature = "embed-assets")]
static EMBEDDED_MANIFEST: OnceLock<Option<AssetManifest>> = OnceLock::new();

/// Register the embedded `static/` tree as the process-wide asset source.
///
/// Parses the embedded `.autumn-manifest.json` so [`asset_url`] and
/// [`is_manifest_asset`] resolve against it. Called by the framework during
/// `AppBuilder::run` before the router is built; calling it more than once is a
/// no-op (the first registration wins).
#[cfg(feature = "embed-assets")]
pub fn register_embedded_static(dir: EmbeddedStaticDir) {
    let _ = EMBEDDED_STATIC.set(dir);
    let _ = EMBEDDED_MANIFEST.set(
        dir.0
            .get_file(ASSET_MANIFEST_FILE)
            .and_then(include_dir::File::contents_utf8)
            .and_then(|s| serde_json::from_str::<AssetManifest>(s).ok()),
    );
}

/// The registered embedded `static/` dir, if [`register_embedded_static`] ran.
#[cfg(feature = "embed-assets")]
#[must_use]
pub fn embedded_static_dir() -> Option<EmbeddedStaticDir> {
    EMBEDDED_STATIC.get().copied()
}

/// Look up a logical asset path in the embedded manifest, returning the
/// fingerprinted path when present.
#[cfg(feature = "embed-assets")]
fn embedded_manifest_lookup(path: &str) -> Option<String> {
    EMBEDDED_MANIFEST.get()?.as_ref()?.files.get(path).cloned()
}

/// `true` if `rel_path` is a fingerprinted value in the embedded manifest.
#[cfg(feature = "embed-assets")]
fn embedded_is_manifest_asset(rel_path: &str) -> bool {
    EMBEDDED_MANIFEST
        .get()
        .and_then(Option::as_ref)
        .is_some_and(|m| m.files.values().any(|v| v == rel_path))
}

/// Return the URL for a static asset, fingerprinted in release builds.
///
/// - **Debug builds** (`cargo run` / `autumn dev`): returns `/static/{path}`
///   with no manifest lookup so edits are always visible immediately.
/// - **Release builds** (`cargo build --release`): looks up `path` in the
///   manifest produced by `autumn build --release` and returns the
///   content-hashed URL (e.g. `/static/css/autumn.a1b2c3d4.css`).
///   Falls back to `/static/{path}` when the manifest is absent or the path
///   is not listed, so the app keeps serving without fingerprinted assets.
///
/// # Example
///
/// ```rust,ignore
/// link rel="stylesheet" href=(asset_url("css/autumn.css"));
/// // debug:   /static/css/autumn.css
/// // release: /static/css/autumn.a1b2c3d4.css
/// ```
#[must_use]
pub fn asset_url(path: &str) -> String {
    // When an embedded `static/` tree is registered (single-binary build), the
    // embedded manifest is the *only* source of truth — assets are served from
    // the binary, so a miss means the asset isn't fingerprinted and should use
    // the plain embedded path. Never fall through to a disk sidecar manifest,
    // which could otherwise point at a hashed file that was never baked in.
    #[cfg(feature = "embed-assets")]
    {
        if embedded_static_dir().is_some() {
            return embedded_manifest_lookup(path)
                .map_or_else(|| format!("/static/{path}"), |fp| format!("/static/{fp}"));
        }
    }
    #[cfg(debug_assertions)]
    {
        format!("/static/{path}")
    }
    #[cfg(not(debug_assertions))]
    {
        if let Some(manifest) = load_manifest() {
            if let Some(fingerprinted) = manifest.files.get(path) {
                return format!("/static/{fingerprinted}");
            }
        }
        format!("/static/{path}")
    }
}

/// Returns `true` if `rel_path` (the portion of the URL after `/static/`) is
/// listed as a fingerprinted value in the release asset manifest.
///
/// Gating the `immutable` cache header on manifest membership rather than
/// filename pattern alone ensures that user-authored assets whose names
/// happen to match `<stem>.<8hex>.<ext>` (e.g. `vendor.deadbeef.js`) are
/// never given a year-long cache lifetime.
///
/// Always returns `false` in debug builds — the manifest does not exist there.
// Not `const fn`: the body branches on build profile/feature, and the
// release/embedded arms perform non-const lookups.
#[allow(clippy::missing_const_for_fn)]
pub(crate) fn is_manifest_asset(rel_path: &str) -> bool {
    // When an embedded `static/` tree is registered, its manifest is the sole
    // authority for the immutable-cache decision (and is reachable in debug
    // builds too); never consult a disk sidecar manifest in that case.
    #[cfg(feature = "embed-assets")]
    {
        if embedded_static_dir().is_some() {
            return embedded_is_manifest_asset(rel_path);
        }
    }
    #[cfg(not(debug_assertions))]
    {
        load_manifest()
            .as_ref()
            .is_some_and(|m| m.files.values().any(|v| v == rel_path))
    }
    #[cfg(debug_assertions)]
    {
        let _ = rel_path;
        false
    }
}

/// Returns `true` if the URI path segment looks like a fingerprinted asset
/// by filename convention (`<stem>.<8-hex-chars>.<ext>`).
///
/// Only used in tests; the cache middleware uses [`is_manifest_asset`] for
/// production cache decisions.
#[cfg(test)]
pub(crate) fn is_fingerprinted_path(uri_path: &str) -> bool {
    let filename = uri_path.rsplit('/').next().unwrap_or("");
    let parts: Vec<&str> = filename.split('.').collect();
    if parts.len() < 3 {
        return false;
    }
    let hash_candidate = parts[parts.len() - 2];
    hash_candidate.len() == 8
        && hash_candidate
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Cache-control policy for `/static/*` responses.
///
/// Fingerprinted assets (members of the manifest, embedded or on-disk) get a
/// year-long `immutable` lifetime; everything else gets `must-revalidate` so
/// returning visitors always pick up the latest file after a deploy. Manifest
/// membership — rather than filename pattern — gates the `immutable` policy so a
/// user-authored `vendor.deadbeef.js` is never frozen for a year.
///
/// Applied as a middleware layer over both the on-disk (`ServeDir`) and the
/// embedded static-serving paths so the policy is identical regardless of where
/// the bytes come from.
pub async fn asset_cache_control(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = req.uri().path().to_owned();
    let mut resp = next.run(req).await;
    if path.starts_with("/static/") && resp.status().is_success() {
        let is_immutable = path.strip_prefix("/static/").is_some_and(is_manifest_asset);
        let header = if is_immutable {
            "public, max-age=31536000, immutable"
        } else {
            "public, max-age=0, must-revalidate"
        };
        resp.headers_mut().insert(
            http::header::CACHE_CONTROL,
            http::HeaderValue::from_static(header),
        );
    }
    resp
}

/// Best-effort `Content-Type` for an embedded asset, derived from its
/// extension. Covers the closed set of asset types Autumn apps ship; unknown
/// extensions fall back to `application/octet-stream`.
#[cfg(feature = "embed-assets")]
#[must_use]
pub(crate) fn content_type_for(path: &str) -> &'static str {
    // Lowercase the extension into a small stack buffer (extensions are short
    // and ASCII) to avoid a per-request heap allocation on the serving path.
    let raw = path
        .rsplit('/')
        .next()
        .unwrap_or("")
        .rsplit_once('.')
        .map_or("", |(_, e)| e);
    let mut buf = [0u8; 8];
    let ext = if raw.is_ascii() && raw.len() <= buf.len() {
        buf[..raw.len()].copy_from_slice(raw.as_bytes());
        buf[..raw.len()].make_ascii_lowercase();
        std::str::from_utf8(&buf[..raw.len()]).unwrap_or("")
    } else {
        "" // unknown long/non-ASCII extension → octet-stream
    };
    match ext {
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "html" | "htm" => "text/html; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// Process-wide cache of computed embedded-asset `ETag`s, keyed by the stable
/// `&'static` byte pointer of the asset's contents.
///
/// SHA-256-hashing a whole asset on every request is a hot-path CPU cost for
/// large full `GET`s. Embedded asset bytes are `&'static`, so their start
/// pointer is a stable per-asset identity for the process lifetime — memoizing
/// the tag against it computes the hash once per asset and reuses it thereafter.
/// Two empty assets can share a pointer, but their content hash is identical, so
/// a collision still returns the correct tag.
#[cfg(feature = "embed-assets")]
static EMBEDDED_ETAG_CACHE: OnceLock<Mutex<HashMap<usize, crate::etag::ETag>>> = OnceLock::new();

/// Return the strong content-hash `ETag` for an embedded asset, computing it on
/// the first serve and reusing the cached value thereafter.
///
/// Preserves the exact tag value/format produced by [`embedded_etag`] so
/// `If-Range` validation is unchanged.
#[cfg(feature = "embed-assets")]
fn embedded_etag_cached(bytes: &[u8]) -> crate::etag::ETag {
    let key = bytes.as_ptr() as usize;
    let cache = EMBEDDED_ETAG_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(etag) = cache.lock().unwrap().get(&key) {
        return etag.clone();
    }
    // Compute outside the lock is unnecessary here (hash is cheap relative to
    // contention on a per-asset key), but keep the critical sections short.
    let etag = embedded_etag(bytes);
    cache.lock().unwrap().insert(key, etag.clone());
    etag
}

/// Derive a stable **strong** `ETag` from an embedded asset's bytes.
///
/// A content hash gives embedded assets a meaningful `If-Range` (and
/// conditional-GET) validator: the tag changes only when the bytes change, so a
/// client's cached range stays valid across restarts of the same binary.
#[cfg(feature = "embed-assets")]
fn embedded_etag(bytes: &[u8]) -> crate::etag::ETag {
    use sha2::{Digest as _, Sha256};
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        hex.push(HEX[(b >> 4) as usize] as char);
        hex.push(HEX[(b & 0x0f) as usize] as char);
    }
    crate::etag::ETag::strong(hex)
}

/// Serve a single file from the registered embedded `static/` tree.
///
/// Returns `404` for path-traversal attempts, the framework manifest
/// (`.autumn-manifest.json`, which must never be exposed), missing files, or
/// when no embedded dir is registered. Other files — including legitimate
/// dotfile assets — are served, matching the on-disk `ServeDir` behavior.
///
/// Honours HTTP `Range` requests (RFC 7233) via [`crate::range`]: a satisfiable
/// `Range` yields `206 Partial Content` with `Content-Range`, an unsatisfiable
/// one yields `416`, and every response advertises `Accept-Ranges: bytes`. A
/// strong content-hash `ETag` gates `If-Range`.
#[cfg(feature = "embed-assets")]
fn embedded_response(rel_path: &str, req_headers: &http::HeaderMap) -> axum::response::Response {
    use axum::response::IntoResponse;

    let is_traversal = rel_path
        .split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..");
    let is_manifest = rel_path
        .rsplit('/')
        .next()
        .is_some_and(|name| name == ASSET_MANIFEST_FILE || name == VENDOR_MANIFEST_FILE);
    if is_traversal || is_manifest {
        return http::StatusCode::NOT_FOUND.into_response();
    }
    let Some(dir) = embedded_static_dir() else {
        return http::StatusCode::NOT_FOUND.into_response();
    };
    dir.0.get_file(rel_path).map_or_else(
        || http::StatusCode::NOT_FOUND.into_response(),
        |file| {
            // `contents()` is `&'static [u8]`; wrap it without a per-request
            // copy so a full response still serves the static bytes directly.
            let bytes = bytes::Bytes::from_static(file.contents());
            let total = bytes.len() as u64;
            let etag = embedded_etag_cached(&bytes);
            let validator = crate::range::Validator::new().with_etag(&etag);
            let resolution = crate::range::resolve(req_headers, total, Some(validator));
            let mut response = crate::range::partial_bytes_response(&resolution, bytes);
            let headers = response.headers_mut();
            headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static(content_type_for(rel_path)),
            );
            headers.insert(http::header::ETAG, etag.header_value());
            response
        },
    )
}

/// Axum handler serving `/static/{*path}` from the embedded tree.
#[cfg(feature = "embed-assets")]
pub(crate) async fn serve_embedded(
    axum::extract::Path(path): axum::extract::Path<String>,
    headers: http::HeaderMap,
) -> axum::response::Response {
    embedded_response(&path, &headers)
}

/// A standalone router that serves `/static/*` from the registered embedded
/// tree with the [`asset_cache_control`] policy applied.
///
/// The framework wires the embedded handler directly into the typed
/// application router; this helper exposes the same serving behavior as a
/// self-contained `Router` for tests and embedding into custom setups.
#[cfg(feature = "embed-assets")]
pub fn embedded_static_router() -> axum::Router {
    axum::Router::new()
        .route("/static/{*path}", axum::routing::get(serve_embedded))
        .layer(axum::middleware::from_fn(asset_cache_control))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_url_returns_static_prefix() {
        let url = asset_url("css/autumn.css");
        assert!(
            url.starts_with("/static/"),
            "url must have /static/ prefix: {url}"
        );
        assert!(
            url.contains("autumn.css"),
            "url must contain asset name: {url}"
        );
    }

    #[test]
    fn fingerprinted_path_detected() {
        assert!(is_fingerprinted_path("/static/css/autumn.a1b2c3d4.css"));
        assert!(is_fingerprinted_path("/static/js/app.00000000.js"));
        assert!(is_fingerprinted_path("/static/img/logo.deadbeef.png"));
    }

    #[test]
    fn non_fingerprinted_paths_rejected() {
        assert!(!is_fingerprinted_path("/static/css/autumn.css"));
        assert!(!is_fingerprinted_path("/static/js/htmx.min.js"));
        assert!(!is_fingerprinted_path("/static/img/logo.png"));
        // Hash too short
        assert!(!is_fingerprinted_path("/static/css/autumn.abc.css"));
        // Hash too long
        assert!(!is_fingerprinted_path("/static/css/autumn.a1b2c3d4e5.css"));
        // Hash contains uppercase (not hex-lowercase)
        assert!(!is_fingerprinted_path("/static/css/autumn.A1B2C3D4.css"));
        // No extension
        assert!(!is_fingerprinted_path("/static/file.a1b2c3d4"));
    }

    #[cfg(feature = "maud")]
    #[test]
    fn vendor_manifest_parses_json() {
        let json = r#"{
            "version": "1",
            "assets": {
                "htmx": {
                    "version": "2.0.4",
                    "source": "https://cdn.jsdelivr.net/npm/htmx.org@2.0.4/dist/htmx.min.js",
                    "file": "js/htmx.min.js",
                    "integrity": "sha384-testintegrityhash"
                }
            }
        }"#;
        let manifest: VendorManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.version, "1");
        let htmx = manifest.assets.get("htmx").unwrap();
        assert_eq!(htmx.version, "2.0.4");
        assert_eq!(htmx.file, "js/htmx.min.js");
        assert_eq!(htmx.integrity, "sha384-testintegrityhash");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn render_javascript_tag_has_src_integrity_crossorigin() {
        let asset = VendorAsset {
            version: "2.0.4".into(),
            source: "https://cdn.jsdelivr.net/npm/htmx.org@2.0.4/dist/htmx.min.js".into(),
            file: "js/htmx.min.js".into(),
            integrity: "sha384-abc123".into(),
        };
        let markup = render_javascript_tag(&asset);
        let html = markup.into_string();
        assert!(
            html.contains("src=\"/static/js/htmx.min.js\""),
            "missing src: {html}"
        );
        assert!(
            html.contains("integrity=\"sha384-abc123\""),
            "missing integrity: {html}"
        );
        assert!(
            html.contains("crossorigin=\"anonymous\""),
            "missing crossorigin: {html}"
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn javascript_include_tag_unknown_name_returns_diagnostic_comment() {
        let markup = javascript_include_tag("__nonexistent_pkg__");
        let html = markup.into_string();
        assert!(
            html.contains("<!-- autumn: asset '__nonexistent_pkg__' not found"),
            "expected HTML comment diagnostic for unknown asset, got: {html}"
        );
    }

    #[cfg(feature = "embed-assets")]
    #[test]
    fn content_type_covers_common_assets() {
        assert_eq!(content_type_for("css/app.css"), "text/css; charset=utf-8");
        assert_eq!(
            content_type_for("js/app.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type_for("img/logo.svg"), "image/svg+xml");
        assert_eq!(content_type_for("img/logo.png"), "image/png");
        assert_eq!(content_type_for("fonts/inter.woff2"), "font/woff2");
        assert_eq!(content_type_for("favicon.ico"), "image/x-icon");
        // Unknown / extensionless fall back to octet-stream.
        assert_eq!(content_type_for("data.bin"), "application/octet-stream");
        assert_eq!(content_type_for("LICENSE"), "application/octet-stream");
    }
}
