//! Mounting a sandboxed artifact as an ordinary Autumn [`Plugin`].
//!
//! A [`SandboxedPlugin`] is installed exactly like a native one:
//!
//! ```rust,no_run
//! # use std::path::Path;
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let plugin = autumn_web::plugin_sandbox::SandboxedPlugin::from_file(
//!     Path::new("plugins/hello.autumn-plugin"),
//! )?;
//! let app = autumn_web::app().plugin(plugin);
//! # let _ = app;
//! # Ok(())
//! # }
//! ```
//!
//! …and from the application's point of view that is where the resemblance
//! ends. A native plugin's `build` receives the whole `AppBuilder`. This one
//! receives nothing: the mount is performed by the framework, from the
//! manifest, and consists of exactly one `nest` at the declared prefix plus the
//! declared route metadata.
//!
//! # The manifest is the mount
//!
//! The router is built from the manifest's `[[routes]]`, one axum route per
//! declared `(method, path)`. A request to an undeclared path under the prefix
//! is a 404 the guest never sees, and a request to a path *outside* the prefix
//! never reaches this router at all. So "which routes it mounts" is a property
//! the runtime enforces, not a claim the artifact makes.
//!
//! # What a misbehaving plugin costs
//!
//! | Its behaviour | What the caller gets | What the rest of the app gets |
//! | --- | --- | --- |
//! | answers | its answer, minus any header it may not set | nothing |
//! | traps, exits, answers badly | 502 on its own prefix | nothing |
//! | spins or floods output | 504 on its own prefix | nothing |
//! | is already at its concurrency ceiling | 503 with `Retry-After` | nothing |
//! | is sent a body over its ceiling | 413, guest never started | nothing |
//!
//! The last column is the point. Every failure mode is scoped to the plugin's
//! own prefix, because the interpreter runs on a blocking worker with a bounded
//! permit count and can only ever return a value.
// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::borrow::Cow;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::response::{IntoResponse as _, Response};
use axum::routing::MethodFilter;
use http::{HeaderName, HeaderValue, StatusCode};
use tokio::sync::Semaphore;

use super::artifact::{ArtifactError, SandboxArtifact};
use super::host::{SandboxHost, SandboxLoadError};
use super::manifest::SandboxManifest;
use super::wire::SandboxRequest;
use crate::app::AppBuilder;
use crate::plugin::Plugin;

/// Response header naming the sandboxed plugin that produced a response.
///
/// Sandboxed responses are otherwise indistinguishable from the host's own, and
/// an operator reading a trace should never have to guess which side of the
/// boundary a byte came from.
pub const SANDBOX_ATTRIBUTION_HEADER: &str = "x-autumn-sandboxed";

/// Why a sandboxed plugin could not be installed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SandboxPluginError {
    /// The artifact could not be read or verified.
    Artifact(ArtifactError),
    /// The module could not be loaded into the sandbox.
    Load(SandboxLoadError),
}

impl fmt::Display for SandboxPluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Artifact(err) => write!(f, "{err}"),
            Self::Load(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for SandboxPluginError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Artifact(err) => Some(err),
            Self::Load(err) => Some(err),
        }
    }
}

impl From<ArtifactError> for SandboxPluginError {
    fn from(err: ArtifactError) -> Self {
        Self::Artifact(err)
    }
}

impl From<SandboxLoadError> for SandboxPluginError {
    fn from(err: SandboxLoadError) -> Self {
        Self::Load(err)
    }
}

/// A sandboxed plugin, ready to mount.
pub struct SandboxedPlugin {
    host: Arc<SandboxHost>,
    /// One permit per concurrently-executing request, so `max_concurrency ×
    /// memory_bytes` bounds what this plugin can cost the host at any instant.
    permits: Arc<Semaphore>,
}

impl fmt::Debug for SandboxedPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxedPlugin")
            .field("plugin", &self.host.manifest().name)
            .field("prefix", &self.host.manifest().prefix)
            .finish_non_exhaustive()
    }
}

impl SandboxedPlugin {
    /// Wrap an already-compiled host.
    #[must_use]
    pub fn new(host: SandboxHost) -> Self {
        let permits = Arc::new(Semaphore::new(host.manifest().limits.max_concurrency));
        Self {
            host: Arc::new(host),
            permits,
        }
    }

    /// Load a verified artifact.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxPluginError::Load`] if the module does not compile or
    /// imports something the sandbox does not provide.
    pub fn from_artifact(artifact: &SandboxArtifact) -> Result<Self, SandboxPluginError> {
        Ok(Self::new(SandboxHost::load(artifact)?))
    }

    /// Read, verify and load a `.autumn-plugin` file.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxPluginError::Artifact`] if the file is missing,
    /// malformed, or does not match its declared digest, and
    /// [`SandboxPluginError::Load`] if the module cannot be sandboxed.
    pub fn from_file(path: &Path) -> Result<Self, SandboxPluginError> {
        Self::from_artifact(&SandboxArtifact::read_file(path)?)
    }

    /// The manifest this plugin is mounted under.
    #[must_use]
    pub fn manifest(&self) -> &SandboxManifest {
        self.host.manifest()
    }

    /// This plugin, mounted at the prefix its own manifest declares.
    ///
    /// The public door, for a test or an embedder that wants the routes without
    /// an [`AppBuilder`]. It nests at `self.manifest().prefix` and nowhere else:
    /// a caller that could choose the prefix could mount a plugin over a
    /// namespace its manifest never named, and the prefix would stop being a
    /// containment boundary.
    ///
    /// Generic over the router's state because the handlers use none — a
    /// sandboxed plugin has no access to application state, which is the point.
    ///
    /// Prefer installing it as a [`Plugin`], which also logs the capability
    /// grant and declares the routes for `autumn routes` and
    /// `autumn plugin-check`.
    #[must_use]
    pub fn mounted_router<S>(&self) -> axum::Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        axum::Router::new().nest(&self.manifest().prefix.clone(), self.router())
    }

    /// The router for this plugin's declared routes, with paths **relative to
    /// the declared prefix** — the shape [`AppBuilder::nest`] expects.
    fn router<S>(&self) -> axum::Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        let manifest = self.manifest();
        let mut router = axum::Router::new();
        for route in &manifest.routes {
            let nested = nested_path(&route.path, &manifest.prefix);
            let Some(filter) = method_filter(&route.method) else {
                // `SandboxManifest` validation already refused every method
                // outside the allowed set, so this is unreachable in practice.
                // Skipping rather than panicking keeps a future manifest
                // vocabulary from turning into a boot crash.
                tracing::error!(
                    plugin = manifest.name,
                    method = route.method,
                    "sandboxed plugin declared a method this build cannot mount; skipping it"
                );
                continue;
            };
            let host = Arc::clone(&self.host);
            let permits = Arc::clone(&self.permits);
            let pattern = route.path.clone();
            router = router.route(
                &nested,
                axum::routing::on(
                    filter,
                    move |params: axum::extract::RawPathParams, request: axum::extract::Request| {
                        let host = Arc::clone(&host);
                        let permits = Arc::clone(&permits);
                        let pattern = pattern.clone();
                        async move { serve(host, permits, pattern, params, request).await }
                    },
                ),
            );
        }
        router
    }
}

impl Plugin for SandboxedPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(self.manifest().name.clone())
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        let manifest = self.manifest().clone();
        // The grant reaches the operator's log before the plugin serves its
        // first request, so "what did we agree to run" is answerable from a
        // production log alone, without the artifact in hand.
        tracing::info!(
            plugin = manifest.name,
            version = manifest.version,
            prefix = manifest.prefix,
            sha256 = manifest.sha256,
            capabilities = manifest
                .capabilities
                .iter()
                .map(|capability| capability.as_str())
                .collect::<Vec<_>>()
                .join(","),
            routes = manifest.routes.len(),
            fuel = manifest.limits.fuel,
            memory_bytes = manifest.limits.memory_bytes,
            max_concurrency = manifest.limits.max_concurrency,
            "mounting a sandboxed plugin under its declared capability grant"
        );
        app.nest(&manifest.prefix, self.router())
            .declare_plugin_routes(manifest.route_infos())
    }
}

/// The path a declared route takes inside a router nested at `prefix`.
///
/// A route *at* the prefix nests as `/`, which is how axum spells "the mount
/// point itself".
fn nested_path(path: &str, prefix: &str) -> String {
    match path.strip_prefix(prefix) {
        Some("") | None => "/".to_owned(),
        Some(rest) => rest.to_owned(),
    }
}

fn method_filter(method: &str) -> Option<MethodFilter> {
    match method {
        "GET" => Some(MethodFilter::GET),
        "HEAD" => Some(MethodFilter::HEAD),
        "POST" => Some(MethodFilter::POST),
        "PUT" => Some(MethodFilter::PUT),
        "PATCH" => Some(MethodFilter::PATCH),
        "DELETE" => Some(MethodFilter::DELETE),
        "OPTIONS" => Some(MethodFilter::OPTIONS),
        _ => None,
    }
}

/// Serve one request through the sandbox.
async fn serve(
    host: Arc<SandboxHost>,
    permits: Arc<Semaphore>,
    pattern: String,
    params: axum::extract::RawPathParams,
    request: axum::extract::Request,
) -> Response {
    let manifest = host.manifest();
    let limits = manifest.limits;
    let plugin = manifest.name.clone();

    // Refuse rather than queue. An unbounded queue in front of a bounded
    // interpreter converts a slow plugin into unbounded host memory; a 503 with
    // `Retry-After` says exactly what happened and costs nothing to produce.
    let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
        tracing::warn!(
            plugin,
            max_concurrency = limits.max_concurrency,
            "sandboxed plugin is at its concurrency ceiling; shedding the request"
        );
        let mut response = sandbox_error(&plugin, StatusCode::SERVICE_UNAVAILABLE);
        response
            .headers_mut()
            .insert(http::header::RETRY_AFTER, HeaderValue::from_static("1"));
        return response;
    };

    let path_params: Vec<(String, String)> = params
        .iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();

    let (parts, body) = request.into_parts();
    // The nested router rewrote the URI, so the concrete path the caller asked
    // for comes from the original.
    let path = parts
        .extensions
        .get::<axum::extract::OriginalUri>()
        .map_or_else(
            || parts.uri.path().to_owned(),
            |original| original.0.path().to_owned(),
        );
    let query = parts.uri.query().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect();

    // The ceiling is applied while reading, so an oversized body is refused
    // without ever being buffered in full.
    let Ok(body) = axum::body::to_bytes(body, limits.max_request_body_bytes).await else {
        tracing::warn!(
            plugin,
            max_request_body_bytes = limits.max_request_body_bytes,
            "request body over the sandboxed plugin's declared ceiling; refusing it"
        );
        return sandbox_error(&plugin, StatusCode::PAYLOAD_TOO_LARGE);
    };
    let body = body.to_vec();

    let sandbox_request = SandboxRequest {
        method: parts.method.as_str().to_owned(),
        route: pattern,
        path,
        query,
        path_params,
        headers,
        body,
    };

    // `wasmi` is a synchronous interpreter: running it on the async runtime
    // would block a worker for the whole fuel budget. `spawn_blocking` also
    // means a panic anywhere in the host shim surfaces as a `JoinError` here
    // instead of unwinding through the request task.
    // The permit moves INTO the closure. A `spawn_blocking` task is never
    // cancelled — dropping its handle detaches it — so a permit held by this
    // future would be released the instant a client disconnects while the
    // interpreter kept running. `max_concurrency` would then bound nothing: a
    // client that connects and immediately resets, in a loop, would fill the
    // shared blocking pool with interpreters nobody is waiting for.
    let outcome = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        host.run(&sandbox_request)
    })
    .await;

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::error!(
                plugin,
                error = %err,
                "the sandbox worker did not complete; serving 502 on the plugin's prefix"
            );
            return sandbox_error(&plugin, StatusCode::BAD_GATEWAY);
        }
    };

    match outcome.result {
        Ok(response) => build_response(&plugin, &response),
        Err(failure) => {
            tracing::warn!(
                plugin,
                failure = %failure,
                stderr = outcome.stderr,
                fuel_used = outcome.fuel_used,
                "sandboxed plugin failed to answer; serving an error on its own prefix"
            );
            sandbox_error(&plugin, failure.status())
        }
    }
}

/// Turn a sanitized guest answer into an HTTP response.
fn build_response(plugin: &str, response: &super::wire::SandboxResponse) -> Response {
    let Ok(status) = StatusCode::from_u16(response.status) else {
        // `SandboxResponse::validate` already refused anything outside the HTTP
        // range, so this cannot happen; refusing again is cheaper than trusting.
        return sandbox_error(plugin, StatusCode::BAD_GATEWAY);
    };

    let mut out = Response::builder().status(status);
    let mut saw_content_type = false;
    for (name, value) in &response.headers {
        let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) else {
            return sandbox_error(plugin, StatusCode::BAD_GATEWAY);
        };
        if name == http::header::CONTENT_TYPE {
            saw_content_type = true;
        }
        out = out.header(name, value);
    }
    if !saw_content_type {
        // A body with no declared type is a body a browser will guess at.
        // Guessing is how a plugin that can only return bytes turns into one
        // that can return a script.
        out = out.header(http::header::CONTENT_TYPE, "application/octet-stream");
    }
    let Ok(mut built) = out.body(Body::from(response.body.clone())) else {
        return sandbox_error(plugin, StatusCode::BAD_GATEWAY);
    };
    stamp_host_headers(&mut built, plugin);
    built
}

fn attribution(plugin: &str) -> Option<HeaderValue> {
    HeaderValue::try_from(plugin).ok()
}

/// Stamp the headers the host owns, replacing anything of the same name.
///
/// `Response::builder().header(..)` *appends*, and `HeaderMap::get` returns the
/// first value — so a guest that emitted `x-autumn-sandboxed` or
/// `x-content-type-options` of its own would win the lookup against the host's.
/// The response header allowlist already refuses both names; this is the second
/// lock on the same door, and it is what makes "every response on this prefix
/// is attributable" true of the 413 and 503 the guest never ran for.
fn stamp_host_headers(response: &mut Response, plugin: &str) {
    let headers = response.headers_mut();
    headers.insert(
        http::header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Some(value) = attribution(plugin) {
        headers.insert(HeaderName::from_static(SANDBOX_ATTRIBUTION_HEADER), value);
    }
}

/// The error a sandboxed plugin's own prefix serves. Never leaks a guest's
/// stderr or trap text to the caller — that is for the operator's log.
fn sandbox_error(plugin: &str, status: StatusCode) -> Response {
    let mut response = (
        status,
        [(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "the sandboxed plugin could not serve this request\n",
    )
        .into_response();
    stamp_host_headers(&mut response, plugin);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_sandbox::manifest::ResourceLimits;
    use crate::plugin_sandbox::test_guests as guests;
    use axum::body::Body;
    use http::{Request, StatusCode};
    use tower::ServiceExt as _;

    fn manifest_toml(routes: &str, limits: &str) -> String {
        format!(
            r#"
name = "autumn-plugin-hello"
version = "0.1.0"
wire_version = 1
prefix = "/hello"
capabilities = ["http-request"]
sha256 = "{digest}"
{routes}
[limits]
{limits}
"#,
            digest = "a".repeat(64)
        )
    }

    fn plugin_from(wat: &str, routes: &str, limits: ResourceLimits) -> SandboxedPlugin {
        let mut manifest = SandboxManifest::parse(&manifest_toml(
            routes,
            "fuel = 200000000\nmemory_bytes = 33554432",
        ))
        .expect("valid manifest");
        manifest.limits = limits;
        let wasm = wat::parse_str(wat).expect("valid WAT");
        SandboxedPlugin::new(SandboxHost::from_module(manifest, &wasm).expect("loads"))
    }

    const GREET_ROUTE: &str = "[[routes]]\nmethod = \"GET\"\npath = \"/hello/greet\"\n";

    fn hello_plugin() -> SandboxedPlugin {
        plugin_from(guests::HELLO, GREET_ROUTE, ResourceLimits::default())
    }

    /// The app a test drives: a sandboxed plugin mounted beside an ordinary
    /// route, so "the rest of the app keeps serving" is something a request
    /// can actually prove.
    fn app(plugin: &SandboxedPlugin) -> axum::Router {
        axum::Router::new()
            .route("/healthz", axum::routing::get(|| async { "ok" }))
            .merge(plugin.mounted_router())
    }

    async fn send(app: axum::Router, method: &str, path: &str) -> (StatusCode, String) {
        send_with_body(app, method, path, Body::empty()).await
    }

    async fn send_with_body(
        app: axum::Router,
        method: &str,
        path: &str,
        body: Body,
    ) -> (StatusCode, String) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .body(body)
            .expect("request");
        let response = app.oneshot(request).await.expect("infallible");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn a_sandboxed_plugin_serves_http_under_its_declared_prefix() {
        let plugin = hello_plugin();
        let (status, body) = send(app(&plugin), "GET", "/hello/greet").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "hello from the sandbox");
    }

    #[tokio::test]
    async fn the_manifest_is_the_mount_not_a_description_of_it() {
        // An undeclared path under the prefix is a 404 from the *host*: the
        // guest is never started, so the manifest bounds what the artifact can
        // serve rather than merely documenting it.
        let plugin = hello_plugin();
        let (status, _) = send(app(&plugin), "GET", "/hello/undeclared").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(app(&plugin), "POST", "/hello/greet").await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn responses_are_attributable_to_the_plugin_that_produced_them() {
        let plugin = hello_plugin();
        let request = Request::builder()
            .uri("/hello/greet")
            .body(Body::empty())
            .expect("request");
        let response = app(&plugin).oneshot(request).await.expect("infallible");
        assert_eq!(
            response
                .headers()
                .get(SANDBOX_ATTRIBUTION_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("autumn-plugin-hello")
        );
    }

    #[tokio::test]
    async fn a_runaway_plugin_5xxs_on_its_own_prefix_while_the_app_keeps_serving() {
        let plugin = plugin_from(
            guests::CPU_SPIN,
            GREET_ROUTE,
            ResourceLimits {
                fuel: 5_000_000,
                ..ResourceLimits::default()
            },
        );
        let (status, _) = send(app(&plugin), "GET", "/hello/greet").await;
        assert!(status.is_server_error(), "{status}");

        let (status, body) = send(app(&plugin), "GET", "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn a_trapping_plugin_never_takes_the_host_with_it() {
        let plugin = plugin_from(guests::TRAP, GREET_ROUTE, ResourceLimits::default());
        let app = app(&plugin);
        let (status, _) = send(app.clone(), "GET", "/hello/greet").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        // The process is still here, and so is every other route.
        let (status, _) = send(app, "GET", "/healthz").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_plugin_that_exits_never_exits_the_host() {
        let plugin = plugin_from(guests::EXIT, GREET_ROUTE, ResourceLimits::default());
        let app = app(&plugin);
        let (status, _) = send(app.clone(), "GET", "/hello/greet").await;
        assert!(status.is_server_error(), "{status}");
        let (status, _) = send(app, "GET", "/healthz").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_body_over_the_declared_ceiling_never_reaches_the_guest() {
        let plugin = plugin_from(
            guests::HELLO,
            "[[routes]]\nmethod = \"POST\"\npath = \"/hello/greet\"\n",
            ResourceLimits {
                max_request_body_bytes: 16,
                ..ResourceLimits::default()
            },
        );
        let (status, _) = send_with_body(
            app(&plugin),
            "POST",
            "/hello/greet",
            Body::from(vec![b'x'; 4096]),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn the_concurrency_ceiling_is_enforced_rather_than_queued() {
        let plugin = plugin_from(
            guests::HELLO,
            GREET_ROUTE,
            ResourceLimits {
                max_concurrency: 1,
                ..ResourceLimits::default()
            },
        );
        // Hold the plugin's only permit, exactly as an in-flight request would.
        let held = std::sync::Arc::clone(&plugin.permits)
            .try_acquire_owned()
            .expect("the first permit is free");
        let (status, _) = send(app(&plugin), "GET", "/hello/greet").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        drop(held);
        let (status, _) = send(app(&plugin), "GET", "/hello/greet").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[test]
    fn the_plugin_is_named_by_its_manifest() {
        assert_eq!(
            crate::plugin::Plugin::name(&hello_plugin()),
            "autumn-plugin-hello"
        );
    }

    #[test]
    fn mounting_declares_the_manifest_routes_with_plugin_attribution() {
        let builder = crate::app::app().plugin(hello_plugin());
        let routes = builder.plugin_route_infos().expect("route manifest");
        let sandboxed: Vec<_> = routes
            .iter()
            .filter(|route| {
                route.source
                    == crate::route_listing::RouteSource::Plugin("autumn-plugin-hello".to_owned())
            })
            .collect();
        assert_eq!(sandboxed.len(), 1);
        assert_eq!(sandboxed[0].path, "/hello/greet");
        assert_eq!(sandboxed[0].method, "GET");
    }

    #[test]
    fn a_mounted_sandboxed_plugin_passes_the_conformance_checks() {
        let builder = crate::app::app().plugin(hello_plugin());
        let routes = builder.plugin_route_infos().expect("route manifest");
        let report = crate::plugin_conformance::run_conformance(
            &crate::plugin_conformance::ConformanceConfig::new("autumn-plugin-hello")
                .prefix("/hello"),
            &routes,
        );
        assert!(report.passed(), "{}", report.to_text_report());
    }
}
