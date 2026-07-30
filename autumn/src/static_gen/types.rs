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
    /// out of `sitemap.xml` — otherwise the app would advertise a URL it also
    /// asks crawlers not to index.
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticManifest {
    /// ISO-8601 timestamp of when the build ran.
    pub generated_at: String,
    /// Autumn framework version that produced this manifest.
    pub autumn_version: String,
    /// Map from URL path (e.g. `"/about"`) to the generated file entry.
    pub routes: HashMap<String, ManifestEntry>,
}

/// A single entry inside a [`StaticManifest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Relative filesystem path to the generated HTML file
    /// (e.g. `"about/index.html"`).
    pub file: String,
    /// Optional ISR revalidation interval in seconds, copied from
    /// [`StaticRouteMeta::revalidate`].
    pub revalidate: Option<u64>,
}

impl StaticManifest {
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
        routes.insert(
            "/".to_owned(),
            ManifestEntry {
                file: "index.html".to_owned(),
                revalidate: None,
            },
        );
        routes.insert(
            "/about".to_owned(),
            ManifestEntry {
                file: "about/index.html".to_owned(),
                revalidate: Some(3600),
            },
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
