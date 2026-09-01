//! Core types for the static generation engine.
//!
//! This module defines the vocabulary used to describe statically generated routes,
//! such as `StaticRouteMeta` (metadata about a route) and `StaticManifest` (the JSON
//! ledger of all files generated during the build).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

/// A set of path parameter values for a parameterized static route.
///
/// Maps parameter names (e.g. `"slug"`) to their values (e.g. `"hello-world"`).
///
/// # Example
///
/// ```
/// use autumn_web::static_gen::StaticParams;
///
/// let mut params = StaticParams::new();
/// params.insert("slug".to_owned(), "hello-world".to_owned());
/// ```
pub type StaticParams = HashMap<String, String>;

/// Convenience macro for building a [`StaticParams`] map.
///
/// # Example
///
/// ```
/// use autumn_web::static_params;
///
/// let params = static_params! { "slug" => "hello-world" };
/// assert_eq!(params.get("slug").unwrap(), "hello-world");
/// ```
#[macro_export]
macro_rules! static_params {
    ($($key:expr => $value:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut map = ::std::collections::HashMap::new();
        $(map.insert($key.to_owned(), $value.to_owned());)*
        map
    }};
}

/// The type-erased async function that returns parameter sets for a
/// parameterized static route.
///
/// This is the type stored inside [`StaticRouteMeta::params_fn`]. The
/// build engine calls it to enumerate all parameter combinations that
/// should be pre-rendered.
pub type ParamsFn = fn(axum::Router) -> Pin<Box<dyn Future<Output = Vec<StaticParams>> + Send>>;

/// Metadata for a route that should be statically generated at build time.
///
/// Used by the `#[static_get]` proc macro to register routes for the
/// static-site build step. The `revalidate` field controls ISR
/// (Incremental Static Regeneration): if set, the pre-rendered page
/// will be refreshed after the given number of seconds.
#[derive(Clone)]
pub struct StaticRouteMeta {
    /// The URL path pattern, e.g. `"/"` or `"/posts/{slug}"`.
    pub path: &'static str,
    /// The handler function name (used for diagnostics and manifest keys).
    pub name: &'static str,
    /// Optional ISR revalidation interval in seconds.
    /// `None` means the page is generated once and never refreshed.
    pub revalidate: Option<u64>,
    /// Optional async function that returns parameter sets for
    /// parameterized routes. `None` for simple (non-parameterized) routes.
    pub params_fn: Option<ParamsFn>,
    /// SEO meta tag defaults declared via the route attribute's `seo(...)`
    /// argument (#1182).
    ///
    /// Carried here as well as on [`Route`](crate::Route) so the sitemap
    /// builder can honour a declared `robots = "noindex…"` and leave the page
    /// out of `sitemap.xml` — otherwise Autumn would advertise a URL it also
    /// asks crawlers not to index.
    ///
    /// This governs the paths Autumn derives from static routes. URLs supplied
    /// by a [`SitemapSource`](crate::seo::SitemapSource) the application
    /// registered are passed through unfiltered; see
    /// `seo::assemble_seo_bodies` for the reasoning.
    pub seo: crate::seo::SeoRouteDefaults,
}

impl std::fmt::Debug for StaticRouteMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticRouteMeta")
            .field("path", &self.path)
            .field("name", &self.name)
            .field("revalidate", &self.revalidate)
            .field("params_fn", &self.params_fn.as_ref().map(|_| "..."))
            .field("seo", &self.seo)
            .finish()
    }
}

/// Persistent manifest written by `autumn build` and read at runtime
/// by the static-file middleware.
///
/// Stored as JSON alongside the generated HTML files.
///
/// `#[non_exhaustive]`: build one with [`StaticManifest::new`] rather than a
/// struct literal, for the same reason as [`ManifestEntry`] — the manifest
/// format is expected to grow, and a literal breaks each time it does.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StaticManifest {
    /// When the build ran: Unix-epoch seconds as a decimal string (e.g.
    /// `"1774555920"`). Stamped by [`StaticManifest::new`]; override it with
    /// [`with_generated_at`](StaticManifest::with_generated_at) to pin a
    /// reproducible build.
    pub generated_at: String,
    /// Autumn framework version that produced this manifest.
    pub autumn_version: String,
    /// Map from URL path (e.g. `"/about"`) to the generated file entry.
    pub routes: HashMap<String, ManifestEntry>,
}

/// A single entry inside a [`StaticManifest`].
///
/// `#[non_exhaustive]`: build one with [`ManifestEntry::new`] plus the
/// `with_*` setters rather than a struct literal. The entry gained a field in
/// #1832 and may gain more; the constructor keeps downstream code compiling
/// when it does.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ManifestEntry {
    /// Relative filesystem path to the generated HTML file
    /// (e.g. `"about/index.html"`).
    pub file: String,
    /// Optional ISR revalidation interval in seconds, copied from
    /// [`StaticRouteMeta::revalidate`].
    ///
    /// `#[serde(default)]` so a hand-written manifest may omit the key
    /// entirely — the serve path documents hand-written manifests as a
    /// supported way to map a route straight at a file, and requiring an
    /// explicit `"revalidate": null` made the shortest such entry fail to
    /// parse (which silently disables the whole static layer).
    ///
    /// Deliberately *not* `skip_serializing_if`, so what `autumn build` writes
    /// keeps the exact shape it had before #1832 — `revalidate` has always been
    /// present in generated manifests, and nothing is gained by dropping it.
    /// That matters for any consumer of `dist/manifest.json` that is not this
    /// struct: a rollback, a rolling deploy sharing one `dist/` volume, or
    /// external tooling with a stricter reader.
    ///
    /// It is *not* needed for an older Autumn runtime, which reads either form:
    /// serde's derive maps a missing `Option` field to `None` even without
    /// `#[serde(default)]`, so omitting the key would not have failed there.
    /// `content_type` is skipped when absent for the same
    /// keep-the-format-boring reason in the other direction — it is a new key,
    /// so not writing it leaves pre-#1832 manifests byte-identical.
    #[serde(default)]
    pub revalidate: Option<u64>,
    /// The `Content-Type` the route's handler declared when this page was
    /// generated (#1832).
    ///
    /// The static-first middleware serves this value verbatim, so the intended
    /// MIME type never has to be reverse-engineered from the route slug and the
    /// served file name. That reverse-engineering was the root cause of three
    /// consecutive edge-case rounds on #1819: generated `.txt`/`.xml` routes are
    /// stored as `robots.txt/index.html` (file name says HTML), while
    /// dotted-slug pages like `/posts/release.v1` are HTML despite a
    /// dot-suffixed route. Recording the type at the one place it is actually
    /// known makes both misreadings impossible.
    ///
    /// `None` means "nothing recorded", which happens in two cases:
    ///
    /// - the manifest predates #1832 (an existing `dist/` that has not been
    ///   rebuilt), or was written by hand; or
    /// - the handler declared no `Content-Type`, so there was no *intended*
    ///   type to record and a build-time guess would only bake in the same
    ///   heuristic this field exists to remove.
    ///
    /// In both cases the serve path falls back to deriving the type from the
    /// route extension and then the served file name — the pre-#1832 behaviour,
    /// unchanged. See
    /// [`resolved_content_type`](crate::static_gen::resolved_content_type).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

impl ManifestEntry {
    /// A manifest entry for `file` with no revalidation interval and no
    /// recorded `Content-Type`.
    ///
    /// The only way to build one from outside the crate: [`ManifestEntry`] is
    /// `#[non_exhaustive]`, so a new field can never again break a downstream
    /// struct literal.
    ///
    /// # Example
    ///
    /// ```
    /// use autumn_web::static_gen::ManifestEntry;
    ///
    /// let entry = ManifestEntry::new("about/index.html")
    ///     .with_content_type(Some("text/html; charset=utf-8".to_owned()));
    /// assert_eq!(entry.file, "about/index.html");
    /// ```
    #[must_use]
    pub fn new(file: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            revalidate: None,
            content_type: None,
        }
    }

    /// Set the ISR revalidation interval, in seconds.
    #[must_use]
    pub const fn with_revalidate(mut self, revalidate: Option<u64>) -> Self {
        self.revalidate = revalidate;
        self
    }

    /// Set the `Content-Type` recorded for this page at generation time.
    #[must_use]
    pub fn with_content_type(mut self, content_type: Option<String>) -> Self {
        self.content_type = content_type;
        self
    }
}

impl StaticManifest {
    /// A manifest for `routes`, stamped with the current time and the running
    /// Autumn version.
    ///
    /// The only way to build one from outside the crate: [`StaticManifest`] is
    /// `#[non_exhaustive]`, so a new field can never break a downstream struct
    /// literal.
    #[must_use]
    pub fn new(routes: HashMap<String, ManifestEntry>) -> Self {
        Self {
            generated_at: unix_timestamp_now(),
            autumn_version: env!("CARGO_PKG_VERSION").to_owned(),
            routes,
        }
    }

    /// Override the build timestamp [`new`](Self::new) stamped.
    ///
    /// [`new`](Self::new) stamps the current time, which a reproducible-build
    /// tool must be able to pin. This is the chainable way to do that; the
    /// field stays `pub`, so assigning it through a `mut` binding also works.
    #[must_use]
    pub fn with_generated_at(mut self, generated_at: impl Into<String>) -> Self {
        self.generated_at = generated_at.into();
        self
    }

    /// Load a manifest from a JSON file on disk.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or contains invalid JSON.
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = std::fs::read_to_string(path)?;
        let manifest: Self = serde_json::from_str(&contents)?;
        Ok(manifest)
    }
}

/// Convert a URL path to the corresponding filesystem path for a
/// statically generated HTML file.
///
/// # Rules
///
/// | URL path | File path |
/// |----------|-----------|
/// | `/` | `index.html` |
/// | `/about` | `about/index.html` |
/// | `/about/` | `about/index.html` |
/// | `/posts/hello` | `posts/hello/index.html` |
#[must_use]
pub fn url_to_file_path(url_path: &str) -> String {
    let trimmed = url_path.trim_matches('/');
    if trimmed.is_empty() {
        "index.html".to_owned()
    } else {
        format!("{trimmed}/index.html")
    }
}

/// Seconds since the Unix epoch, as a decimal string (avoids pulling in
/// chrono/time just to stamp a manifest).
fn unix_timestamp_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn url_to_file_path_root() {
        assert_eq!(url_to_file_path("/"), "index.html");
    }

    #[test]
    fn url_to_file_path_simple() {
        assert_eq!(url_to_file_path("/about"), "about/index.html");
    }

    #[test]
    fn url_to_file_path_nested() {
        assert_eq!(url_to_file_path("/posts/hello"), "posts/hello/index.html");
    }

    #[test]
    fn url_to_file_path_trailing_slash() {
        assert_eq!(url_to_file_path("/about/"), "about/index.html");
    }

    #[test]
    fn manifest_roundtrip() {
        let mut routes = HashMap::new();
        routes.insert("/".to_owned(), ManifestEntry::new("index.html".to_owned()));
        routes.insert(
            "/about".to_owned(),
            ManifestEntry::new("about/index.html".to_owned()).with_revalidate(Some(3600)),
        );

        let manifest = StaticManifest {
            generated_at: "2026-03-27T12:00:00Z".to_owned(),
            autumn_version: "0.3.0".to_owned(),
            routes,
        };

        // Serialize to JSON
        let json = serde_json::to_string(&manifest).expect("serialize");

        // Write to a temp file, then load back via StaticManifest::load
        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("manifest.json");
        {
            let mut f = std::fs::File::create(&file_path).expect("create file");
            f.write_all(json.as_bytes()).expect("write");
        }

        let loaded = StaticManifest::load(&file_path).expect("load");

        assert_eq!(loaded.generated_at, "2026-03-27T12:00:00Z");
        assert_eq!(loaded.autumn_version, "0.3.0");
        assert_eq!(loaded.routes.len(), 2);

        let root_entry = loaded.routes.get("/").expect("root route");
        assert_eq!(root_entry.file, "index.html");
        assert!(root_entry.revalidate.is_none());

        let about_entry = loaded.routes.get("/about").expect("about route");
        assert_eq!(about_entry.file, "about/index.html");
        assert_eq!(about_entry.revalidate, Some(3600));
    }

    // ── #1832: intended Content-Type recorded at generation time ────────────

    #[test]
    fn manifest_entry_roundtrips_recorded_content_type() {
        let mut routes = HashMap::new();
        routes.insert(
            "/feed.xml".to_owned(),
            ManifestEntry::new("feed.xml/index.html".to_owned())
                .with_content_type(Some("application/rss+xml".to_owned())),
        );
        let manifest = StaticManifest {
            generated_at: "2026-09-01T00:00:00Z".to_owned(),
            autumn_version: "0.6.0".to_owned(),
            routes,
        };

        let json = serde_json::to_string(&manifest).expect("serialize");
        let loaded: StaticManifest = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(
            loaded.routes["/feed.xml"].content_type.as_deref(),
            Some("application/rss+xml"),
            "the recorded Content-Type must survive a manifest round-trip"
        );
    }

    #[test]
    fn legacy_manifest_without_content_type_deserializes_to_none() {
        // A `dist/` built by a pre-#1832 Autumn has no `content_type` key.
        // It must keep loading, with the field defaulting to `None` so the
        // serve path falls back to its derivation instead of failing.
        let json = r#"{
            "generated_at": "2026-03-27T12:00:00Z",
            "autumn_version": "0.3.0",
            "routes": {
                "/about": { "file": "about/index.html", "revalidate": null }
            }
        }"#;

        let loaded: StaticManifest = serde_json::from_str(json).expect("legacy manifest loads");
        let entry = loaded.routes.get("/about").expect("about route");
        assert_eq!(entry.file, "about/index.html");
        assert!(
            entry.content_type.is_none(),
            "a legacy entry must default to no recorded Content-Type"
        );
    }

    #[test]
    fn manifest_entry_omits_absent_content_type_from_json() {
        // Keep the on-disk manifest readable by older runtimes (and small):
        // an entry with nothing recorded serializes without the key at all.
        let entry = ManifestEntry::new("about/index.html".to_owned());
        let json = serde_json::to_string(&entry).expect("serialize");
        assert!(
            !json.contains("content_type"),
            "absent Content-Type must not be written to the manifest, got {json}"
        );
    }

    /// A manifest written by *this* Autumn must stay loadable by a **pre-#1832**
    /// runtime, which declares `revalidate` with no `#[serde(default)]` and so
    /// treats a missing key as a hard error. Omitting it would fail
    /// `StaticManifest::load` there, `StaticFileLayer::new` would return `None`,
    /// and every pre-rendered page would silently serve dynamically — breaking
    /// a rollback or a rolling deploy sharing one `dist/`.
    #[test]
    fn manifest_entry_always_writes_revalidate_for_older_runtimes() {
        let json =
            serde_json::to_string(&ManifestEntry::new("about/index.html")).expect("serialize");
        assert!(
            json.contains("\"revalidate\""),
            "revalidate must stay present so generated manifests keep the shape \
             they had before #1832, got {json}"
        );

        // `content_type` is a new key, so omitting it when absent keeps a
        // manifest byte-identical to what pre-#1832 `autumn build` wrote.
        assert!(
            !json.contains("content_type"),
            "absent content_type must still be omitted, got {json}"
        );
    }

    #[test]
    fn manifest_entry_builders_compose() {
        let entry = ManifestEntry::new("feed.xml/index.html")
            .with_revalidate(Some(600))
            .with_content_type(Some("application/rss+xml".to_owned()));
        assert_eq!(entry.file, "feed.xml/index.html");
        assert_eq!(entry.revalidate, Some(600));
        assert_eq!(entry.content_type.as_deref(), Some("application/rss+xml"));

        let bare = ManifestEntry::new("index.html");
        assert_eq!(bare.file, "index.html");
        assert!(bare.revalidate.is_none());
        assert!(bare.content_type.is_none());
    }

    #[test]
    fn static_route_meta_clone() {
        let meta = StaticRouteMeta {
            path: "/test",
            name: "test_handler",
            revalidate: Some(60),
            params_fn: None,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
        };
        let copy = meta.clone();
        // Use original after clone to prove it's a real copy, not a move
        assert_eq!(meta.path, copy.path);
        assert_eq!(copy.name, "test_handler");
        assert_eq!(copy.revalidate, Some(60));
    }

    #[test]
    fn static_params_macro() {
        let params = static_params! { "slug" => "hello-world" };
        assert_eq!(params.get("slug").unwrap(), "hello-world");
    }

    #[test]
    fn static_params_macro_multiple() {
        let params = static_params! {
            "year" => "2026",
            "month" => "03",
            "slug" => "hello",
        };
        assert_eq!(params.len(), 3);
        assert_eq!(params.get("year").unwrap(), "2026");
        assert_eq!(params.get("month").unwrap(), "03");
        assert_eq!(params.get("slug").unwrap(), "hello");
    }

    #[test]
    fn static_params_macro_empty() {
        let params: StaticParams = static_params! {};
        assert!(params.is_empty());
    }

    #[test]
    fn static_route_meta_with_params_fn() {
        fn dummy_params(
            _router: axum::Router,
        ) -> Pin<Box<dyn Future<Output = Vec<StaticParams>> + Send>> {
            Box::pin(async { vec![static_params! { "slug" => "test" }] })
        }

        let meta = StaticRouteMeta {
            path: "/posts/{slug}",
            name: "show_post",
            revalidate: None,
            params_fn: Some(dummy_params),
            seo: crate::seo::SeoRouteDefaults::EMPTY,
        };
        assert!(meta.params_fn.is_some());
        assert_eq!(meta.path, "/posts/{slug}");
    }

    #[test]
    fn static_route_meta_debug() {
        let meta = StaticRouteMeta {
            path: "/test",
            name: "test",
            revalidate: None,
            params_fn: None,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
        };
        let debug = format!("{meta:?}");
        assert!(debug.contains("test"));
    }
}
