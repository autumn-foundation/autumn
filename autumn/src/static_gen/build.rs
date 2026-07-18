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
        futures::stream::iter(jobs.iter().map(|job| {
            let router = router.clone();
            let staging = staging.clone();
            let url = job.url.clone();
            let revalidate = job.revalidate;
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
                    ManifestEntry {
                        file: file_path,
                        revalidate,
                    },
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
    let manifest = StaticManifest {
        generated_at: timestamp_now(),
        autumn_version: env!("CARGO_PKG_VERSION").to_owned(),
        routes: manifest_routes,
    };
    let json = serde_json::to_string_pretty(&manifest)?;
    tokio::fs::write(staging.join("manifest.json"), json).await?;

    // Atomic swap: remove old dist, rename staging -> dist
    if tokio::fs::try_exists(dist_dir).await.unwrap_or(false) {
        tokio::fs::remove_dir_all(dist_dir).await?;
    }
    tokio::fs::rename(&staging, dist_dir).await?;

    Ok(())
}

/// Simple Unix-epoch timestamp (avoids pulling in chrono/time).
fn timestamp_now() -> String {
    let now = std::time::SystemTime::now();
    let secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}")
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
        };

        let result = render_static_routes(echo_router(), &[meta], &dist).await;
        assert!(result.is_ok());

        let manifest = StaticManifest::load(&dist.join("manifest.json")).unwrap();
        let entry = manifest.routes.get("/posts/hello").unwrap();
        assert_eq!(entry.revalidate, Some(3600));
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
