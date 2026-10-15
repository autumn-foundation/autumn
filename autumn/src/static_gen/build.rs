//! Static build renderer.
//!
//! Renders `#[static_get]` routes through the Axum router and writes
//! the output HTML to a staging directory, then atomically swaps to
//! `dist/`. This is the engine behind `autumn build`.

use std::collections::HashMap;
use std::path::Path;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use tower::ServiceExt;

use super::{ManifestEntry, StaticManifest, StaticParams, StaticRouteMeta, url_to_file_path};

/// Default number of routes rendered concurrently.
const DEFAULT_CONCURRENCY: usize = 8;

/// Errors that can occur during static rendering.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BuildError {
    /// A route handler returned a non-2xx HTTP status.
    #[error("Route {path} returned HTTP {status} (expected 2xx)")]
    NonSuccessStatus {
        /// The URL path that failed.
        path: String,
        /// The HTTP status code returned.
        status: StatusCode,
    },

    /// Failed to read the response body from a route handler.
    #[error("Failed to read response body for {path}: {source}")]
    BodyRead {
        /// The URL path whose body could not be read.
        path: String,
        /// The underlying Axum error.
        source: axum::Error,
    },

    /// An I/O error occurred while writing files.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON serialization error occurred while writing the manifest.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// The params function for a parameterized route returned an empty list.
    #[error("Params function for route {path} returned no parameter sets")]
    EmptyParams {
        /// The URL path pattern that had no params.
        path: String,
    },
}

/// A concrete URL path to render, produced by expanding parameterized routes.
struct RenderJob {
    /// The concrete URL path (e.g. `/posts/hello`).
    url: String,
    /// Optional ISR revalidation interval.
    revalidate: Option<u64>,
    /// `Content-Type` declared on the route attribute (#2403).
    content_type: Option<&'static str>,
}

/// Expand a `StaticRouteMeta` into one or more concrete `RenderJob`s.
///
/// For simple routes (no `params_fn`), returns a single job with the literal path.
/// For parameterized routes, calls the params function and substitutes each
/// parameter set into the path pattern.
async fn expand_route(
    meta: &StaticRouteMeta,
    router: &axum::Router,
) -> Result<Vec<RenderJob>, BuildError> {
    match meta.params_fn {
        None => {
            // Simple static route -- single job
            Ok(vec![RenderJob {
                url: meta.path.to_owned(),
                revalidate: meta.revalidate,
                content_type: meta.content_type,
            }])
        }
        Some(params_fn) => {
            // Parameterized route -- call the params function
            let param_sets = params_fn(router.clone()).await;
            if param_sets.is_empty() {
                return Err(BuildError::EmptyParams {
                    path: meta.path.to_owned(),
                });
            }

            let jobs = param_sets
                .into_iter()
                .map(|params| {
                    let url = substitute_params(meta.path, &params);
                    RenderJob {
                        url,
                        revalidate: meta.revalidate,
                        content_type: meta.content_type,
                    }
                })
                .collect();

            Ok(jobs)
        }
    }
}

/// Substitute parameter values into a URL path pattern.
///
/// Replaces `{name}` placeholders with the corresponding value from the params map.
///
/// # Example
///
/// ```text
/// substitute_params("/posts/{slug}", {"slug": "hello"}) => "/posts/hello"
/// substitute_params("/blog/{year}/{slug}", {"year": "2026", "slug": "hi"}) => "/blog/2026/hi"
/// ```
fn substitute_params(pattern: &str, params: &StaticParams) -> String {
    let mut result = pattern.to_owned();
    for (key, value) in params {
        let placeholder = format!("{{{key}}}");
        result = result.replace(&placeholder, value);
    }
    result
}

/// The `Content-Type` to record for `url`, given the rendered response's headers
/// (#1832) — or `None` when there is no usable declared type.
///
/// Returns `None` when the handler declared no `Content-Type`, or the value
/// fails [`usable_recorded_content_type`](super::middleware::usable_recorded_content_type)
/// (the same screen the serve path uses).
///
/// Otherwise it records
/// [`effective_content_type`](super::middleware::effective_content_type): an
/// axum return-type default (`String`, `Vec<u8>`) on a route with a
/// disagreeing asset extension becomes the extension type. So
/// `#[static_get("/theme.css")] async fn theme() -> String` records
/// `text/css; charset=utf-8`, not `text/plain` (which `nosniff` blocks).
/// ISR applies the same function to fresh responses, so the type guard still
/// holds on these routes (#2409).
///
/// A type declared on the route (#2403) is recorded as is, if usable. It is
/// intent, so the extension never replaces it, and the second value is `true`.
fn recorded_content_type(
    headers: &axum::http::HeaderMap,
    url: &str,
    declared: Option<&str>,
) -> (Option<String>, bool) {
    if let Some(value) = super::middleware::usable_recorded_content_type(declared) {
        return (Some(value.to_owned()), true);
    }
    (inferred_content_type(headers, url), false)
}

/// The type the handler's response declared, normalized for `url`.
fn inferred_content_type(headers: &axum::http::HeaderMap, url: &str) -> Option<String> {
    let raw = headers
        .get(axum::http::header::CONTENT_TYPE)?
        .to_str()
        .ok()?;
    let value = super::middleware::usable_recorded_content_type(Some(raw))?;
    Some(super::middleware::effective_content_type(value, url).to_owned())
}

/// Render all static routes and write them to `dist_dir`.
///
/// Routes are rendered concurrently (up to `DEFAULT_CONCURRENCY` at a time)
/// using `buffer_unordered`.
///
/// For parameterized routes, the params function is called first to expand
/// each route pattern into concrete URLs. For example,
/// `/posts/{slug}` with params `["hello", "world"]` becomes two render jobs:
/// `/posts/hello` and `/posts/world`.
///
/// 1. Expands parameterized routes into concrete render jobs.
/// 2. Renders to a staging directory (`{dist_dir}.staging`).
/// 3. On success, atomically renames staging -> dist.
/// 4. On failure, removes staging and returns the first error.
///
/// If `dist_dir` already exists, it is replaced.
///
/// # Errors
///
/// Returns [`BuildError`] if:
/// - Any route handler returns a non-2xx HTTP status.
/// - A response body cannot be read.
/// - An I/O error occurs while writing files or swapping directories.
/// - The manifest cannot be serialized to JSON.
/// - A params function returns an empty list.
///
/// # Panics
///
/// Panics if the Axum `Request` builder produces an invalid request
/// (should never happen with valid `StaticRouteMeta` paths) or if the
/// router's `oneshot` service returns an error (Axum routers are
/// infallible).
pub async fn render_static_routes(
    router: axum::Router,
    metas: &[StaticRouteMeta],
    dist_dir: &Path,
) -> Result<(), BuildError> {
    // Phase 1: Expand all routes into concrete render jobs
    let mut jobs = Vec::new();
    for meta in metas {
        let expanded = expand_route(meta, &router).await?;
        eprintln!("  Route {} -> {} page(s)", meta.path, expanded.len());
        jobs.extend(expanded);
    }

    let staging = dist_dir.with_extension("staging");

    // Clean staging dir if it exists from a previous failed build
    if tokio::fs::try_exists(&staging).await.unwrap_or(false) {
        tokio::fs::remove_dir_all(&staging).await?;
    }
    tokio::fs::create_dir_all(&staging).await?;

    // Pre-create all subdirectories (avoids races between concurrent tasks)
    for job in &jobs {
        let file_path = url_to_file_path(&job.url);
        let full_path = staging.join(&file_path);
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }

    // Render concurrently
    let results: Vec<Result<(String, ManifestEntry), BuildError>> =
        // Consume `jobs` by value: a `|job: &RenderJob|` closure makes rustc
        // infer a single concrete region for the returned `async move` block,
        // and the `Send` check then fails with "implementation of `FnOnce` is
        // not general enough", poisoning every future that awaits this fn.
        futures::stream::iter(jobs.into_iter().map(|job| {
            let router = router.clone();
            let staging = staging.clone();
            let url = job.url;
            let revalidate = job.revalidate;
            let declared = job.content_type;
            async move {
                eprintln!("  Rendering {url} ...");

                let response = router
                    .oneshot(
                        Request::builder()
                            .uri(&url)
                            // Internal build render: exempt from the inbound
                            // request-timeout deadline (no client connection).
                            .extension(super::RenderDeadlineExempt)
                            .body(Body::empty())
                            .expect("valid request"),
                    )
                    .await
                    .expect("router infallible");

                if !response.status().is_success() {
                    return Err(BuildError::NonSuccessStatus {
                        path: url,
                        status: response.status(),
                    });
                }

                // #1832: capture the Content-Type the handler declared, before
                // the response is consumed for its body. This is the one place
                // the page's intended MIME type is actually known; recording it
                // is what frees the serve path from re-deriving it from the
                // route slug and the `<route>/index.html` file name.
                let (content_type, explicit) =
                    recorded_content_type(response.headers(), &url, declared);

                let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .map_err(|e| BuildError::BodyRead {
                        path: url.clone(),
                        source: e,
                    })?;

                let file_path = url_to_file_path(&url);
                // staging dir pre-created above, just write
                let full_path = staging.join(&file_path);
                tokio::fs::write(&full_path, &body_bytes).await?;

                Ok((
                    url,
                    ManifestEntry::new(file_path)
                        .with_revalidate(revalidate)
                        .with_content_type(content_type)
                        .with_content_type_explicit(explicit),
                ))
            }
        }))
        .buffer_unordered(DEFAULT_CONCURRENCY)
        .collect()
        .await;

    // Check for errors -- if any route failed, clean up and return first error
    let mut manifest_routes = HashMap::new();
    for result in results {
        match result {
            Ok((path, entry)) => {
                manifest_routes.insert(path, entry);
            }
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&staging).await;
                return Err(e);
            }
        }
    }

    // Write manifest
    let manifest = StaticManifest::new(manifest_routes);
    let json = serde_json::to_string_pretty(&manifest)?;
    tokio::fs::write(staging.join("manifest.json"), json).await?;

    // Atomic swap: remove old dist, rename staging -> dist
    if tokio::fs::try_exists(dist_dir).await.unwrap_or(false) {
        tokio::fs::remove_dir_all(dist_dir).await?;
    }
    tokio::fs::rename(&staging, dist_dir).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::static_gen::StaticRouteMeta;
    use std::future::Future;
    use std::pin::Pin;

    fn test_meta(path: &'static str, name: &'static str) -> StaticRouteMeta {
        StaticRouteMeta {
            path,
            name,
            revalidate: None,
            params_fn: None,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            content_type: None,
        }
    }

    fn test_meta_with_revalidate(
        path: &'static str,
        name: &'static str,
        revalidate: u64,
    ) -> StaticRouteMeta {
        StaticRouteMeta {
            path,
            name,
            revalidate: Some(revalidate),
            params_fn: None,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            content_type: None,
        }
    }

    // --- ParamsFn helpers for tests ---
    // Since ParamsFn is a fn pointer (not a closure), we define named
    // functions that return fixed parameter sets for each test scenario.

    fn slug_params_hello_world(
        _router: axum::Router,
    ) -> Pin<Box<dyn Future<Output = Vec<StaticParams>> + Send>> {
        Box::pin(async {
            vec![
                crate::static_params! { "slug" => "hello" },
                crate::static_params! { "slug" => "world" },
            ]
        })
    }

    fn slug_params_alpha_beta(
        _router: axum::Router,
    ) -> Pin<Box<dyn Future<Output = Vec<StaticParams>> + Send>> {
        Box::pin(async {
            vec![
                crate::static_params! { "slug" => "alpha" },
                crate::static_params! { "slug" => "beta" },
            ]
        })
    }

    fn multi_params(
        _router: axum::Router,
    ) -> Pin<Box<dyn Future<Output = Vec<StaticParams>> + Send>> {
        Box::pin(async {
            vec![
                crate::static_params! { "year" => "2026", "slug" => "hello" },
                crate::static_params! { "year" => "2025", "slug" => "world" },
            ]
        })
    }

    fn slug_params_hello(
        _router: axum::Router,
    ) -> Pin<Box<dyn Future<Output = Vec<StaticParams>> + Send>> {
        Box::pin(async { vec![crate::static_params! { "slug" => "hello" }] })
    }

    /// Router whose handler declares `text/html; charset=utf-8`, the shape a
    /// real `#[static_get]` page handler (returning `Markup`) produces.
    /// Regression: the render future must be provably `Send` so `run()` can be
    /// spawned. A by-reference job closure breaks this at compile time.
    #[test]
    fn render_static_routes_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let fut = render_static_routes(axum::Router::new(), &[], Path::new("unused"));
        assert_send(&fut);
    }

    fn html_router() -> axum::Router {
        axum::Router::new().fallback(axum::routing::get(|uri: axum::http::Uri| async move {
            axum::response::Html(format!("<h1>{}</h1>", uri.path()))
        }))
    }

    fn echo_router() -> axum::Router {
        axum::Router::new().fallback(axum::routing::get(|uri: axum::http::Uri| async move {
            format!("Hello from {}", uri.path())
        }))
    }

    // --- Simple route tests (Phase 1 regression) ---

    #[tokio::test]
    async fn renders_single_route_to_dist() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        let result =
            render_static_routes(echo_router(), &[test_meta("/about", "about")], &dist).await;
        assert!(result.is_ok(), "render failed: {:?}", result.err());
        let html = std::fs::read_to_string(dist.join("about/index.html")).unwrap();
        assert_eq!(html, "Hello from /about");
        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(manifest.routes.len(), 1);
        assert!(manifest.routes.contains_key("/about"));
    }

    #[tokio::test]
    async fn renders_root_route() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        let result = render_static_routes(echo_router(), &[test_meta("/", "index")], &dist).await;
        assert!(result.is_ok());
        let html = std::fs::read_to_string(dist.join("index.html")).unwrap();
        assert_eq!(html, "Hello from /");
    }

    #[tokio::test]
    async fn rejects_non_2xx_response() {
        let router =
            axum::Router::new().fallback(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") });
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        let result = render_static_routes(router, &[test_meta("/about", "about")], &dist).await;
        assert!(result.is_err());
        assert!(!dist.exists(), "dist should not exist after failed build");
        let staging = dist.with_extension("staging");
        assert!(
            !staging.exists(),
            "staging dir should be cleaned up after failed build"
        );
    }

    #[tokio::test]
    async fn cleans_stale_dist_before_build() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("stale.html"), "old").unwrap();
        let result =
            render_static_routes(echo_router(), &[test_meta("/about", "about")], &dist).await;
        assert!(result.is_ok());
        assert!(!dist.join("stale.html").exists());
        assert!(dist.join("about/index.html").exists());
    }

    #[tokio::test]
    async fn renders_multiple_routes_concurrently() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        let result = render_static_routes(
            echo_router(),
            &[
                test_meta("/", "index"),
                test_meta("/about", "about"),
                test_meta("/contact", "contact"),
            ],
            &dist,
        )
        .await;
        assert!(result.is_ok());
        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(manifest.routes.len(), 3);
        // Verify all files exist
        assert!(dist.join("index.html").exists());
        assert!(dist.join("about/index.html").exists());
        assert!(dist.join("contact/index.html").exists());
    }

    // --- Parameterized route tests (Phase 2) ---

    #[test]
    fn substitute_params_single() {
        let params = crate::static_params! { "slug" => "hello-world" };
        let result = substitute_params("/posts/{slug}", &params);
        assert_eq!(result, "/posts/hello-world");
    }

    #[test]
    fn substitute_params_multiple() {
        let params = crate::static_params! {
            "year" => "2026",
            "slug" => "hello",
        };
        let result = substitute_params("/blog/{year}/{slug}", &params);
        assert_eq!(result, "/blog/2026/hello");
    }

    #[test]
    fn substitute_params_no_placeholders() {
        let params = StaticParams::new();
        let result = substitute_params("/about", &params);
        assert_eq!(result, "/about");
    }

    #[tokio::test]
    async fn renders_parameterized_route() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");

        let meta = StaticRouteMeta {
            path: "/posts/{slug}",
            name: "show_post",
            revalidate: None,
            params_fn: Some(slug_params_hello_world),
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            content_type: None,
        };

        let result = render_static_routes(echo_router(), &[meta], &dist).await;
        assert!(result.is_ok(), "render failed: {:?}", result.err());

        // Verify both pages generated
        let hello_html = std::fs::read_to_string(dist.join("posts/hello/index.html")).unwrap();
        assert_eq!(hello_html, "Hello from /posts/hello");

        let world_html = std::fs::read_to_string(dist.join("posts/world/index.html")).unwrap();
        assert_eq!(world_html, "Hello from /posts/world");

        // Verify manifest
        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(manifest.routes.len(), 2);
        assert!(manifest.routes.contains_key("/posts/hello"));
        assert!(manifest.routes.contains_key("/posts/world"));
    }

    #[tokio::test]
    async fn renders_multi_param_route() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");

        let meta = StaticRouteMeta {
            path: "/blog/{year}/{slug}",
            name: "blog_post",
            revalidate: None,
            params_fn: Some(multi_params),
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            content_type: None,
        };

        let result = render_static_routes(echo_router(), &[meta], &dist).await;
        assert!(result.is_ok(), "render failed: {:?}", result.err());

        assert!(dist.join("blog/2026/hello/index.html").exists());
        assert!(dist.join("blog/2025/world/index.html").exists());

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(manifest.routes.len(), 2);
        assert!(manifest.routes.contains_key("/blog/2026/hello"));
        assert!(manifest.routes.contains_key("/blog/2025/world"));
    }

    #[tokio::test]
    async fn mixed_simple_and_parameterized_routes() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");

        let metas = vec![
            test_meta("/", "index"),
            test_meta("/about", "about"),
            StaticRouteMeta {
                path: "/posts/{slug}",
                name: "show_post",
                revalidate: None,
                params_fn: Some(slug_params_alpha_beta),
                seo: crate::seo::SeoRouteDefaults::EMPTY,
                content_type: None,
            },
        ];

        let result = render_static_routes(echo_router(), &metas, &dist).await;
        assert!(result.is_ok(), "render failed: {:?}", result.err());

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        // 2 simple + 2 parameterized = 4 total
        assert_eq!(manifest.routes.len(), 4);
        assert!(manifest.routes.contains_key("/"));
        assert!(manifest.routes.contains_key("/about"));
        assert!(manifest.routes.contains_key("/posts/alpha"));
        assert!(manifest.routes.contains_key("/posts/beta"));
    }

    #[tokio::test]
    async fn parameterized_route_manifest_includes_revalidate() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");

        let meta = StaticRouteMeta {
            path: "/posts/{slug}",
            name: "show_post",
            revalidate: Some(3600),
            params_fn: Some(slug_params_hello),
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            content_type: None,
        };

        let result = render_static_routes(echo_router(), &[meta], &dist).await;
        assert!(result.is_ok());

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        let entry = manifest.routes.get("/posts/hello").unwrap();
        assert_eq!(entry.revalidate, Some(3600));
    }

    // ── #1832: the intended Content-Type is captured at generation time ─────

    /// A handler that declares `application/xml` has that exact type recorded
    /// in the manifest, even though the page is stored as
    /// `sitemap.xml/index.html` — the file name the serve-time heuristic used
    /// to have to reverse-engineer.
    #[tokio::test]
    async fn records_handler_declared_content_type_in_manifest() {
        fn xml_router() -> axum::Router {
            axum::Router::new().fallback(axum::routing::get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/xml")],
                    "<urlset/>",
                )
            }))
        }

        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        let result =
            render_static_routes(xml_router(), &[test_meta("/sitemap.xml", "sitemap")], &dist)
                .await;
        assert!(result.is_ok(), "render failed: {:?}", result.err());

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        let entry = manifest.routes.get("/sitemap.xml").expect("sitemap route");
        assert_eq!(entry.file, "sitemap.xml/index.html");
        assert_eq!(
            entry.content_type.as_deref(),
            Some("application/xml"),
            "the handler's declared Content-Type must be recorded at generation time"
        );
    }

    /// A type the serve-time extension heuristic could never produce
    /// (`application/rss+xml` from an extensionless `/feed` route) round-trips
    /// intact.
    #[tokio::test]
    async fn records_content_type_unreachable_by_extension_heuristic() {
        fn rss_router() -> axum::Router {
            axum::Router::new().fallback(axum::routing::get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/rss+xml")],
                    "<rss/>",
                )
            }))
        }

        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        render_static_routes(rss_router(), &[test_meta("/feed", "feed")], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(
            manifest.routes["/feed"].content_type.as_deref(),
            Some("application/rss+xml")
        );
    }

    /// A parameterized route records the declared type on every expanded page.
    #[tokio::test]
    async fn records_content_type_for_each_parameterized_page() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        let meta = StaticRouteMeta {
            path: "/posts/{slug}",
            name: "show_post",
            revalidate: None,
            params_fn: Some(slug_params_hello_world),
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            content_type: None,
        };

        render_static_routes(html_router(), &[meta], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        for route in ["/posts/hello", "/posts/world"] {
            assert_eq!(
                manifest.routes[route].content_type.as_deref(),
                Some("text/html; charset=utf-8"),
                "{route} must carry the declared Content-Type"
            );
        }
    }

    /// When the handler declares no `Content-Type` at all there is nothing
    /// *intended* to record: the entry stays `None` so the serve path keeps
    /// using its derivation rather than having a guess baked into the manifest.
    #[tokio::test]
    async fn records_no_content_type_when_handler_declares_none() {
        fn bare_router() -> axum::Router {
            axum::Router::new().fallback(axum::routing::get(|| async {
                axum::response::Response::builder()
                    .status(StatusCode::OK)
                    .body(axum::body::Body::from("bare"))
                    .unwrap()
            }))
        }

        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        render_static_routes(bare_router(), &[test_meta("/bare", "bare")], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert!(
            manifest.routes["/bare"].content_type.is_none(),
            "no declared type must record as None, not as a guess"
        );
    }

    /// A `Content-Type` whose bytes are not visible ASCII cannot be written to
    /// a JSON manifest and re-emitted as a header, so it is dropped rather than
    /// recorded — the serve path falls back instead of carrying a broken value.
    #[tokio::test]
    async fn skips_non_ascii_content_type() {
        fn opaque_router() -> axum::Router {
            axum::Router::new().fallback(axum::routing::get(|| async {
                let mut response = axum::response::Response::new(axum::body::Body::from("x"));
                response.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_bytes(b"text/\xffhtml").unwrap(),
                );
                response
            }))
        }

        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        render_static_routes(opaque_router(), &[test_meta("/weird", "weird")], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert!(
            manifest.routes["/weird"].content_type.is_none(),
            "a non-visible-ASCII Content-Type must not be recorded"
        );
    }

    /// A horizontal tab is legal OWS around media-type parameters, so
    /// `application/rss+xml;\tprofile="x"` is a good header and must be recorded
    /// — and, critically, honoured at serve time. Rejecting it would send an
    /// extensionless `/feed` back to the `feed/index.html` derivation and serve
    /// an RSS feed as `text/html` under `nosniff`. Both halves are asserted
    /// here, because recording a value the serve path then discards is worse
    /// than recording nothing.
    #[tokio::test]
    async fn records_content_type_with_legal_tab_ows() {
        fn tabbed_router() -> axum::Router {
            axum::Router::new().fallback(axum::routing::get(|| async {
                let mut response = axum::response::Response::new(axum::body::Body::from("<rss/>"));
                response.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_bytes(b"application/rss+xml;\tprofile=\"x\"")
                        .expect("tab is a legal HeaderValue byte"),
                );
                response
            }))
        }

        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        render_static_routes(tabbed_router(), &[test_meta("/feed", "feed")], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        let recorded = manifest.routes["/feed"].content_type.as_deref();
        assert_eq!(
            recorded,
            Some("application/rss+xml;\tprofile=\"x\""),
            "tab OWS is legal in a media type and must be recorded verbatim"
        );

        // ...and the serve path must honour what generation recorded. If it
        // did not, `/feed` would derive `text/html` from `feed/index.html`.
        assert_eq!(
            super::super::resolved_content_type(
                recorded,
                "/feed",
                std::path::Path::new("feed/index.html"),
            ),
            "application/rss+xml;\tprofile=\"x\"",
            "generation and serving must agree, or the recorded value is dead \
             data and the route silently falls back to the derivation"
        );
    }

    /// Pins the behaviour change the changelog documents: a handler returning a
    /// bare `String` declares `text/plain`, and that is now what gets recorded
    /// (and therefore served), instead of the `text/html` the old serve-time
    /// heuristic assumed for every `<route>/index.html`. Making the change
    /// visible here stops it from being silently "fixed" later.
    #[tokio::test]
    async fn records_text_plain_for_bare_string_handler() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        render_static_routes(echo_router(), &[test_meta("/about", "about")], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(
            manifest.routes["/about"].content_type.as_deref(),
            Some("text/plain; charset=utf-8"),
            "a bare-String handler declares text/plain; record what it declared"
        );
    }

    /// An empty `Content-Type` is not a type. Recording it would put an empty
    /// header in the manifest for the serve path to reject later.
    #[tokio::test]
    async fn skips_empty_content_type() {
        fn empty_ct_router() -> axum::Router {
            axum::Router::new().fallback(axum::routing::get(|| async {
                let mut response = axum::response::Response::new(axum::body::Body::from("x"));
                response.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static(""),
                );
                response
            }))
        }

        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        render_static_routes(empty_ct_router(), &[test_meta("/empty", "empty")], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert!(
            manifest.routes["/empty"].content_type.is_none(),
            "an empty Content-Type must not be recorded"
        );
    }

    // --- Generic return-type defaults must not clobber a route's extension ---

    /// The regression this guard exists for. `#[static_get("/theme.css")]`
    /// returning a `String` declares `text/plain; charset=utf-8` purely because
    /// of the return type. Recording it would serve a stylesheet as plain text,
    /// and `X-Content-Type-Options: nosniff` (on by default) makes the browser
    /// drop it entirely. The extension type is recorded instead (#2409), so the
    /// served type is unchanged and ISR gets a real expectation.
    #[tokio::test]
    async fn records_extension_type_for_generic_text_plain_over_a_recognized_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        render_static_routes(echo_router(), &[test_meta("/theme.css", "theme")], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(
            manifest.routes["/theme.css"].content_type.as_deref(),
            Some("text/css; charset=utf-8"),
            "records the extension type, not the String default"
        );
    }

    /// Same rule for the byte-slice default: `/logo.png` returning `Vec<u8>`
    /// declares `application/octet-stream`, which would block the image.
    #[tokio::test]
    async fn records_extension_type_for_generic_octet_stream_over_a_recognized_extension() {
        fn bytes_router() -> axum::Router {
            axum::Router::new().fallback(axum::routing::get(|| async {
                b"\x89PNG\r\n\x1a\n".to_vec()
            }))
        }

        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        render_static_routes(bytes_router(), &[test_meta("/logo.png", "logo")], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(
            manifest.routes["/logo.png"].content_type.as_deref(),
            Some("image/png"),
            "records the extension type, not the Vec<u8> default"
        );
    }

    /// The guard is about *generic* defaults only. A handler that explicitly
    /// declares a type still wins, even when its slug says otherwise.
    #[tokio::test]
    async fn records_explicit_type_that_contradicts_the_route_extension() {
        fn json_router() -> axum::Router {
            axum::Router::new().fallback(axum::routing::get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    r#"{"notes":[]}"#,
                )
            }))
        }

        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        render_static_routes(json_router(), &[test_meta("/notes.txt", "notes")], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(
            manifest.routes["/notes.txt"].content_type.as_deref(),
            Some("application/json"),
            "an explicit declaration is intent and must be recorded"
        );
    }

    /// The guard fires only when the extension-derived type **disagrees** with
    /// the generic default. `/notes.txt` returning a bare `String` declares
    /// `text/plain; charset=utf-8`, which is exactly what `.txt` derives, so
    /// there is nothing to protect and the value is recorded.
    ///
    /// This is the assertion that distinguishes the `from_extension != value`
    /// comparison from a blanket "generic default on any recognized extension →
    /// record nothing": under the blanket rule this route would record `None`.
    #[tokio::test]
    async fn generic_default_agreeing_with_the_extension_is_recorded() {
        fn plain_router() -> axum::Router {
            axum::Router::new().fallback(axum::routing::get(|| async { "hello".to_owned() }))
        }

        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        render_static_routes(plain_router(), &[test_meta("/notes.txt", "notes")], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(
            manifest.routes["/notes.txt"].content_type.as_deref(),
            Some("text/plain; charset=utf-8"),
            "a generic default that agrees with the route extension has nothing \
             to clobber, so it must be recorded rather than dropped"
        );
    }

    /// And the guard must not swallow a route whose extension is outside the
    /// asset table: `.bin` derives nothing, so there is no preference to honour
    /// and the declared value stands.
    #[tokio::test]
    async fn generic_default_is_recorded_for_an_unrecognized_extension() {
        fn bytes_router() -> axum::Router {
            axum::Router::new().fallback(axum::routing::get(|| async { vec![0u8, 1, 2, 3] }))
        }

        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        render_static_routes(bytes_router(), &[test_meta("/data.bin", "data")], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(
            manifest.routes["/data.bin"].content_type.as_deref(),
            Some("application/octet-stream")
        );
    }

    /// The documented escape hatch from the generic-default guard: only axum's
    /// own two exact spellings are treated as inferred, so a handler that
    /// deliberately wants a plain-text `.css` route declares it distinctly
    /// (bare `text/plain`) and that declaration is recorded.
    #[tokio::test]
    async fn explicitly_distinct_generic_type_is_recorded_on_an_extensioned_route() {
        fn plain_router() -> axum::Router {
            axum::Router::new().fallback(axum::routing::get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/plain")],
                    "not really css",
                )
            }))
        }

        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        render_static_routes(plain_router(), &[test_meta("/theme.css", "theme")], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(
            manifest.routes["/theme.css"].content_type.as_deref(),
            Some("text/plain"),
            "a distinctly-spelled declaration is intent, not an inferred default"
        );
    }

    /// An extension outside the asset table has nothing to prefer, so even an
    /// exact generic default is recorded. (This is why a `/report.pdf` handler
    /// declaring `application/octet-stream` keeps that type.)
    #[tokio::test]
    async fn generic_default_is_recorded_for_an_extension_outside_the_asset_table() {
        fn pdf_router() -> axum::Router {
            axum::Router::new().fallback(axum::routing::get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
                    "%PDF-1.7",
                )
            }))
        }

        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        render_static_routes(pdf_router(), &[test_meta("/report.pdf", "report")], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(
            manifest.routes["/report.pdf"].content_type.as_deref(),
            Some("application/octet-stream"),
            ".pdf is not in the asset table, so there is no extension to prefer"
        );
    }

    // ── #2403: a route-declared Content-Type beats the inferred default ─────

    fn declared(path: &'static str, content_type: &'static str) -> StaticRouteMeta {
        StaticRouteMeta {
            content_type: Some(content_type),
            ..test_meta(path, "declared")
        }
    }

    /// The case the issue names: a deliberate `application/octet-stream`
    /// on `/logo.png` is recorded as declared, not remapped to `image/png`.
    #[tokio::test]
    async fn declared_octet_stream_is_recorded_on_a_png_route() {
        fn bytes_router() -> axum::Router {
            axum::Router::new().fallback(axum::routing::get(|| async {
                b"\x89PNG\r\n\x1a\n".to_vec()
            }))
        }

        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        let meta = declared("/logo.png", "application/octet-stream");
        render_static_routes(bytes_router(), &[meta], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        let entry = &manifest.routes["/logo.png"];
        assert_eq!(
            entry.content_type.as_deref(),
            Some("application/octet-stream")
        );
        assert!(entry.content_type_explicit);
    }

    /// The route declaration wins over what the handler sent.
    #[tokio::test]
    async fn declared_type_wins_over_the_handler_header() {
        fn json_router() -> axum::Router {
            axum::Router::new().fallback(axum::routing::get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    "{}",
                )
            }))
        }

        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        render_static_routes(json_router(), &[declared("/data", "text/csv")], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(
            manifest.routes["/data"].content_type.as_deref(),
            Some("text/csv")
        );
    }

    /// A declared type serves routes whose handler sets no header.
    #[tokio::test]
    async fn declared_type_is_recorded_when_the_handler_sets_none() {
        fn bare_router() -> axum::Router {
            axum::Router::new().fallback(axum::routing::get(|| async {
                axum::response::Response::new(axum::body::Body::from("bare"))
            }))
        }

        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        render_static_routes(
            bare_router(),
            &[declared("/feed", "application/rss+xml")],
            &dist,
        )
        .await
        .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(
            manifest.routes["/feed"].content_type.as_deref(),
            Some("application/rss+xml")
        );
    }

    /// A declared generic type is kept on a route with a disagreeing extension.
    #[tokio::test]
    async fn declared_text_plain_is_recorded_on_a_css_route() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        let meta = declared("/theme.css", "text/plain; charset=utf-8");
        render_static_routes(echo_router(), &[meta], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(
            manifest.routes["/theme.css"].content_type.as_deref(),
            Some("text/plain; charset=utf-8")
        );
    }

    /// Every page of a parameterized route gets the declared type.
    #[tokio::test]
    async fn declared_type_applies_to_each_parameterized_page() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        let meta = StaticRouteMeta {
            path: "/posts/{slug}",
            params_fn: Some(slug_params_hello_world),
            content_type: Some("text/markdown"),
            ..test_meta("/posts/{slug}", "show_post")
        };
        render_static_routes(html_router(), &[meta], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        for route in ["/posts/hello", "/posts/world"] {
            let entry = &manifest.routes[route];
            assert_eq!(entry.content_type.as_deref(), Some("text/markdown"));
            assert!(entry.content_type_explicit);
        }
    }

    /// Padding around a declared value is trimmed when recorded.
    #[tokio::test]
    async fn declared_type_is_trimmed_when_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        let meta = declared("/data", "  text/csv  ");
        render_static_routes(echo_router(), &[meta], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert_eq!(
            manifest.routes["/data"].content_type.as_deref(),
            Some("text/csv")
        );
    }

    /// An inferred type is still not explicit.
    #[tokio::test]
    async fn inferred_type_is_not_marked_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        render_static_routes(echo_router(), &[test_meta("/theme.css", "theme")], &dist)
            .await
            .expect("render");

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        assert!(!manifest.routes["/theme.css"].content_type_explicit);
    }

    #[tokio::test]
    async fn simple_route_with_revalidate() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        let meta = test_meta_with_revalidate("/about", "about", 60);

        let result = render_static_routes(echo_router(), &[meta], &dist).await;
        assert!(result.is_ok());

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        let entry = manifest.routes.get("/about").unwrap();
        assert_eq!(entry.revalidate, Some(60));
    }
}
