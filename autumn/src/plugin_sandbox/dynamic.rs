//! Mount and unmount a sandboxed plugin on a running app (issue #2433).
//!
//! The app does not swap its router. The app's 404 fallback reads a table of
//! mounted plugins. A request that matches a static route never reads the
//! table. Ordinary routes therefore have no added cost.
//!
//! # Rules
//!
//! - A static route wins over a mounted prefix. Use a prefix that no static
//!   route uses.
//! - A framework prefix is refused. The configured actuator and probe paths
//!   count. Add more with [`PluginMounts::reserve`].
//! - One plugin holds a prefix. A prefix that contains, or is inside, a held
//!   prefix is refused.
//! - **Unmount drains.** New requests get 404 immediately. A running request
//!   finishes. It keeps its concurrency permit until it ends.
//! - A new plugin instance at a reused prefix has its own permits. While the
//!   old instance drains, both can run requests.
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

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use axum::extract::{Request, State};
use axum::response::Response;
use tower::ServiceExt as _;

use super::plugin::SandboxedPlugin;
use crate::route_listing::RouteInfo;
use crate::state::AppState;

/// A mounted plugin, as an operator sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MountedPluginInfo {
    /// Plugin name.
    pub name: String,
    /// Plugin version.
    pub version: String,
    /// The prefix it serves.
    pub prefix: String,
    /// Digest of the whole artifact, if it came from one.
    pub artifact_sha256: Option<String>,
    /// Granted capabilities.
    pub capabilities: Vec<String>,
    /// The mount number. Each mount gets a higher number.
    pub generation: u64,
    /// Requests running now.
    pub in_flight: usize,
}

/// Why a plugin could not be mounted.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MountError {
    /// The prefix belongs to the framework.
    Reserved {
        /// The prefix that was asked for.
        prefix: String,
    },
    /// Another plugin holds this prefix, or one that overlaps it.
    PrefixTaken {
        /// The prefix that was asked for.
        prefix: String,
        /// The plugin that holds the overlapping prefix.
        held_by: String,
    },
}

impl fmt::Display for MountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reserved { prefix } => write!(f, "prefix {prefix} belongs to the framework"),
            Self::PrefixTaken { prefix, held_by } => {
                write!(f, "prefix {prefix} overlaps a prefix held by {held_by}")
            }
        }
    }
}

impl std::error::Error for MountError {}

struct Entry {
    plugin: SandboxedPlugin,
    router: axum::Router,
    generation: u64,
}

impl Entry {
    fn info(&self) -> MountedPluginInfo {
        let manifest = self.plugin.manifest();
        MountedPluginInfo {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            prefix: manifest.prefix.clone(),
            artifact_sha256: self.plugin.artifact_sha256().map(str::to_owned),
            capabilities: manifest
                .capabilities
                .iter()
                .map(|capability| capability.as_str().to_owned())
                .collect(),
            generation: self.generation,
            in_flight: self.plugin.in_flight(),
        }
    }
}

#[derive(Default)]
struct Table {
    next_generation: u64,
    reserved: Vec<String>,
    by_prefix: BTreeMap<String, Entry>,
}

/// The table of plugins mounted on a running app. Clones share one table.
///
/// Install it with [`AppBuilder::plugin_mounts`](crate::app::AppBuilder::plugin_mounts).
#[derive(Clone, Default)]
pub struct PluginMounts {
    table: Arc<RwLock<Table>>,
}

/// Prefixes the framework always owns. [`PluginMounts::reserve`] adds more.
const BASE_RESERVED: &[&str] = &[
    "/static",
    "/_autumn",
    "/__autumn",
    "/favicon.ico",
    "/.well-known",
];

/// The prefixes this app configuration gives to the framework.
///
/// The router mounts the normalized actuator prefix, so that is the one to
/// reserve. A root prefix is left out: it would reserve every path.
#[must_use]
pub fn framework_prefixes(config: &crate::config::AutumnConfig) -> Vec<String> {
    let mut reserved = Vec::new();
    let actuator = crate::actuator::normalize_actuator_prefix(&config.actuator.prefix);
    if !actuator.is_empty() {
        reserved.push(actuator);
    }
    // Probes are the framework's only when it mounts them.
    if config.health.enabled {
        reserved.extend([
            config.health.path.clone(),
            config.health.live_path.clone(),
            config.health.ready_path.clone(),
            config.health.startup_path.clone(),
        ]);
    }
    // The inspector runs in the dev profile only.
    if crate::config::profile_is_dev(config.profile.as_deref()) {
        reserved.push(config.dev.inspector_path.clone());
    }
    #[cfg(feature = "maud")]
    if config.stories.enabled {
        reserved.push(crate::stories::STORIES_PATH.to_owned());
    }
    reserved
}

/// A reservation as a rooted path with no trailing slash. `None` for the root,
/// which would reserve every path.
fn normalize_reserved(prefix: &str) -> Option<String> {
    let trimmed = prefix.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        None
    } else if trimmed.starts_with('/') {
        Some(trimmed.to_owned())
    } else {
        Some(format!("/{trimmed}"))
    }
}

/// `true` if one prefix is the other, or lies under it.
fn overlaps(a: &str, b: &str) -> bool {
    is_under(a, b) || is_under(b, a)
}

/// `true` if `path` is `prefix` or lies under it, on a segment boundary.
fn is_under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

impl PluginMounts {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> RwLockReadGuard<'_, Table> {
        self.table.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Table> {
        self.table.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Refuse plugins under these prefixes, such as a custom MCP path.
    ///
    /// [`AppBuilder::plugin_mounts`](crate::app::AppBuilder::plugin_mounts)
    /// already reserves the configured actuator and probe paths.
    ///
    /// A plugin that is already mounted on a reserved prefix is unmounted.
    pub fn reserve<I, S>(&self, prefixes: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut table = self.write();
        for prefix in prefixes.into_iter().map(Into::into) {
            let Some(prefix) = normalize_reserved(&prefix) else {
                continue;
            };
            let clashing: Vec<String> = table
                .by_prefix
                .keys()
                .filter(|held| overlaps(held, &prefix))
                .cloned()
                .collect();
            for held in clashing {
                tracing::warn!(
                    prefix = held,
                    reserved = prefix,
                    "unmounting a plugin on a reserved prefix"
                );
                table.by_prefix.remove(&held);
            }
            table.reserved.push(prefix);
        }
    }

    /// Mount a plugin at its manifest prefix. Returns its generation.
    ///
    /// Logs the capability grant, like a boot-time mount.
    ///
    /// # Errors
    ///
    /// [`MountError::Reserved`] for a framework prefix.
    /// [`MountError::PrefixTaken`] if the prefix overlaps a held one.
    pub fn mount(&self, plugin: &SandboxedPlugin) -> Result<u64, MountError> {
        let prefix = plugin.manifest().prefix.clone();
        let mut table = self.write();
        if BASE_RESERVED
            .iter()
            .copied()
            .chain(table.reserved.iter().map(String::as_str))
            .any(|reserved| overlaps(&prefix, reserved))
        {
            tracing::warn!(prefix, "refusing to mount a plugin on a framework prefix");
            return Err(MountError::Reserved { prefix });
        }
        if let Some(held) = table
            .by_prefix
            .values()
            .find(|entry| overlaps(&prefix, &entry.plugin.manifest().prefix))
            .map(|entry| entry.plugin.manifest().name.clone())
        {
            tracing::warn!(
                prefix,
                held_by = held,
                "refusing to mount an overlapping prefix"
            );
            return Err(MountError::PrefixTaken {
                prefix,
                held_by: held,
            });
        }
        table.next_generation = table.next_generation.saturating_add(1);
        let generation = table.next_generation;
        plugin.log_grant("mounting a sandboxed plugin on a running app", generation);
        table.by_prefix.insert(
            prefix,
            Entry {
                plugin: plugin.clone(),
                // An undeclared path under the prefix gets the framework 404.
                router: plugin
                    .mounted_router()
                    .fallback(crate::middleware::error_page_filter::fallback_404_handler),
                generation,
            },
        );
        drop(table);
        Ok(generation)
    }

    /// Unmount the plugin at `prefix`. New requests get 404 immediately.
    /// Running requests finish.
    pub fn unmount(&self, prefix: &str) -> Option<MountedPluginInfo> {
        let entry = self.write().by_prefix.remove(prefix)?;
        let info = entry.info();
        tracing::info!(
            plugin = info.name,
            prefix = info.prefix,
            generation = info.generation,
            in_flight = info.in_flight,
            "unmounted a sandboxed plugin; running requests will finish"
        );
        Some(info)
    }

    /// What is mounted now, ordered by prefix.
    #[must_use]
    pub fn list(&self) -> Vec<MountedPluginInfo> {
        self.read().by_prefix.values().map(Entry::info).collect()
    }

    /// The routes of every mounted plugin.
    #[must_use]
    pub fn routes(&self) -> Vec<RouteInfo> {
        self.read()
            .by_prefix
            .values()
            .flat_map(|entry| entry.plugin.manifest().route_infos())
            .collect()
    }

    /// `true` when a mounted plugin owns `path`: the router's fallback then
    /// gives it the request before anything the framework generates there.
    pub(crate) fn owns(&self, path: &str) -> bool {
        let table = self.read();
        table
            .by_prefix
            .iter()
            .any(|(prefix, _)| is_under(path, prefix))
    }

    /// The router for the plugin that owns `path`. The clone keeps a running
    /// request alive after an unmount.
    fn router_for(&self, path: &str) -> Option<axum::Router> {
        let table = self.read();
        table
            .by_prefix
            .iter()
            .find(|(prefix, _)| is_under(path, prefix))
            .map(|(_, entry)| entry.router.clone())
    }
}

impl fmt::Debug for PluginMounts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginMounts").finish_non_exhaustive()
    }
}

/// The app's 404 fallback. Serves a mounted plugin, else the usual 404.
#[doc(hidden)]
pub async fn mounts_fallback(State(state): State<AppState>, request: Request) -> Response {
    let router = state
        .extension::<PluginMounts>()
        .and_then(|mounts| mounts.router_for(request.uri().path()));
    if let Some(router) = router {
        return match router.oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        };
    }
    let (method, uri) = (request.method().clone(), request.uri().clone());
    crate::middleware::error_page_filter::fallback_404_handler(method, uri).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_sandbox::host::SandboxHost;
    use crate::plugin_sandbox::manifest::{ResourceLimits, SandboxManifest};
    use crate::plugin_sandbox::test_guests as guests;
    use axum::body::Body;
    use http::{Request as HttpRequest, StatusCode};

    fn plugin(prefix: &str, wat: &str, limits: ResourceLimits) -> SandboxedPlugin {
        let toml = format!(
            "name = \"p{n}\"\nversion = \"0.1.0\"\nwire_version = 1\nprefix = \"{prefix}\"\n\
             capabilities = [\"http-request\"]\nsha256 = \"{d}\"\n\
             [[routes]]\nmethod = \"GET\"\npath = \"{prefix}/greet\"\n\
             [limits]\nfuel = 200000000\nmemory_bytes = 33554432\n",
            n = prefix.replace('/', "-"),
            d = "a".repeat(64),
        );
        let mut manifest = SandboxManifest::parse(&toml).expect("manifest");
        manifest.limits = limits;
        let wasm = wat::parse_str(wat).expect("wat");
        SandboxedPlugin::new(SandboxHost::from_module(manifest, &wasm).expect("loads"))
    }

    fn hello(prefix: &str) -> SandboxedPlugin {
        plugin(prefix, guests::HELLO, ResourceLimits::default())
    }

    /// A router shaped like the app: one static route, mounts on the fallback.
    fn app(mounts: &PluginMounts) -> axum::Router {
        let state = AppState::for_test();
        state.insert_extension(mounts.clone());
        axum::Router::new()
            .route("/healthz", axum::routing::get(|| async { "ok" }))
            .fallback(mounts_fallback)
            .with_state(state)
    }

    async fn get(app: &axum::Router, path: &str) -> (StatusCode, String) {
        let request = HttpRequest::builder()
            .uri(path)
            .body(Body::empty())
            .expect("request");
        let response = app.clone().oneshot(request).await.expect("infallible");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn a_mounted_plugin_answers_without_a_restart() {
        let mounts = PluginMounts::new();
        let app = app(&mounts);
        assert_eq!(get(&app, "/hello/greet").await.0, StatusCode::NOT_FOUND);
        mounts.mount(&hello("/hello")).expect("mounts");
        let (status, body) = get(&app, "/hello/greet").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "hello from the sandbox");
    }

    #[tokio::test]
    async fn an_unmounted_prefix_is_a_404_again() {
        let mounts = PluginMounts::new();
        let app = app(&mounts);
        mounts.mount(&hello("/hello")).expect("mounts");
        assert_eq!(get(&app, "/hello/greet").await.0, StatusCode::OK);
        assert!(mounts.unmount("/hello").is_some());
        assert_eq!(get(&app, "/hello/greet").await.0, StatusCode::NOT_FOUND);
        assert!(mounts.unmount("/hello").is_none(), "already gone");
    }

    #[tokio::test]
    async fn other_routes_are_unaffected_by_mount_and_unmount() {
        let mounts = PluginMounts::new();
        let app = app(&mounts);
        mounts.mount(&hello("/hello")).expect("mounts");
        assert_eq!(get(&app, "/healthz").await, (StatusCode::OK, "ok".into()));
        let _ = mounts.unmount("/hello");
        assert_eq!(get(&app, "/healthz").await, (StatusCode::OK, "ok".into()));
    }

    #[tokio::test]
    async fn a_static_route_wins_over_a_mounted_prefix() {
        let mounts = PluginMounts::new();
        let app = app(&mounts);
        mounts.mount(&hello("/healthz")).expect("mounts");
        assert_eq!(get(&app, "/healthz").await.1, "ok");
    }

    #[tokio::test]
    async fn a_path_that_only_shares_leading_characters_is_not_under_the_prefix() {
        let mounts = PluginMounts::new();
        let app = app(&mounts);
        mounts.mount(&hello("/hello")).expect("mounts");
        assert_eq!(
            get(&app, "/hellothere/greet").await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn a_held_or_overlapping_prefix_is_refused() {
        let mounts = PluginMounts::new();
        mounts.mount(&hello("/a/b")).expect("mounts");
        for prefix in ["/a/b", "/a", "/a/b/c"] {
            assert!(
                matches!(
                    mounts.mount(&hello(prefix)),
                    Err(MountError::PrefixTaken { .. })
                ),
                "{prefix}"
            );
        }
        mounts.mount(&hello("/a/bc")).expect("a sibling is fine");
    }

    #[test]
    fn a_framework_prefix_is_refused() {
        let mounts = PluginMounts::new();
        for prefix in [
            "/static",
            "/_autumn/x",
            "/__autumn/live-reload",
            "/.well-known/x",
        ] {
            assert!(
                matches!(
                    mounts.mount(&hello(prefix)),
                    Err(MountError::Reserved { .. })
                ),
                "{prefix}"
            );
        }
        assert!(mounts.list().is_empty());
        mounts.reserve(["/ops/", "", "/"]);
        assert!(matches!(
            mounts.mount(&hello("/ops/admin")),
            Err(MountError::Reserved { .. })
        ));
        // A prefix that contains a reserved one is refused too.
        mounts.reserve(["/internal/ops"]);
        assert!(matches!(
            mounts.mount(&hello("/internal")),
            Err(MountError::Reserved { .. })
        ));
        mounts.mount(&hello("/staticx")).expect("a sibling name");
    }

    #[test]
    fn the_configured_framework_paths_are_reserved() {
        let mut config = crate::config::AutumnConfig {
            profile: Some("prod".to_owned()),
            ..crate::config::AutumnConfig::default()
        };
        let defaults = framework_prefixes(&config);
        for path in ["/actuator", "/health", "/live", "/ready", "/startup"] {
            assert!(defaults.iter().any(|p| p == path), "{path}: {defaults:?}");
        }
        assert!(!defaults.contains(&config.dev.inspector_path), "prod");

        config.actuator.prefix = "ops/".to_owned();
        assert!(framework_prefixes(&config).contains(&"/ops".to_owned()));

        config.actuator.prefix = "/".to_owned();
        assert!(!framework_prefixes(&config).iter().any(String::is_empty));

        config.health.enabled = false;
        assert!(framework_prefixes(&config).is_empty(), "probes are free");

        config.profile = Some("dev".to_owned());
        assert_eq!(
            framework_prefixes(&config),
            [config.dev.inspector_path.clone()]
        );
    }

    #[test]
    fn reserving_a_prefix_unmounts_a_plugin_already_on_it() {
        let mounts = PluginMounts::new();
        mounts.mount(&hello("/ops")).expect("mounts");
        mounts.reserve(["/ops"]);
        assert!(mounts.list().is_empty());
    }

    #[tokio::test]
    async fn an_app_without_mounts_still_answers_404() {
        let app = axum::Router::new()
            .fallback(mounts_fallback)
            .with_state(AppState::for_test());
        assert_eq!(get(&app, "/hello/greet").await.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn a_released_prefix_can_be_claimed_again_with_a_new_generation() {
        let mounts = PluginMounts::new();
        let first = mounts.mount(&hello("/hello")).expect("mounts");
        let _ = mounts.unmount("/hello");
        let second = mounts.mount(&hello("/hello")).expect("mounts again");
        assert!(second > first);
        assert_eq!(mounts.list()[0].generation, second);
    }

    #[test]
    fn list_and_routes_answer_what_is_running_now() {
        let mounts = PluginMounts::new();
        assert!(mounts.list().is_empty() && mounts.routes().is_empty());
        mounts.mount(&hello("/hello")).expect("mounts");
        let listed = mounts.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].prefix, "/hello");
        assert_eq!(listed[0].capabilities, ["http-request"]);
        assert!(mounts.routes().iter().any(|r| r.path == "/hello/greet"));
        let _ = mounts.unmount("/hello");
        assert!(mounts.list().is_empty() && mounts.routes().is_empty());
    }

    #[tokio::test]
    async fn unmount_drains_and_the_permit_is_accounted() {
        let limits = ResourceLimits {
            max_concurrency: 2,
            ..ResourceLimits::default()
        };
        let plugin = plugin("/hello", guests::HELLO, limits);
        let mounts = PluginMounts::new();
        mounts.mount(&plugin).expect("mounts");
        // A request already running holds a permit, as `serve` does.
        let held = plugin.hold_permit_for_test();
        assert_eq!(mounts.list()[0].in_flight, 1);
        let info = mounts.unmount("/hello").expect("unmounts");
        assert_eq!(info.in_flight, 1, "the running request is still counted");
        assert_eq!(plugin.in_flight(), 1, "unmount did not reclaim the permit");
        drop(held);
        assert_eq!(plugin.in_flight(), 0, "the permit comes back when it ends");
    }

    #[tokio::test]
    async fn a_request_running_at_unmount_still_gets_its_answer() {
        let spin = plugin(
            "/spin",
            guests::CPU_SPIN,
            ResourceLimits {
                fuel: 20_000_000,
                ..ResourceLimits::default()
            },
        );
        let mounts = PluginMounts::new();
        let app = app(&mounts);
        mounts.mount(&spin).expect("mounts");
        let running = tokio::spawn({
            let app = app.clone();
            async move { get(&app, "/spin/greet").await.0 }
        });
        // Unmount only once the request runs, so the test shows a real drain.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while spin.in_flight() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "request never started"
            );
            tokio::task::yield_now().await;
        }
        let _ = mounts.unmount("/spin");
        let status = tokio::time::timeout(std::time::Duration::from_secs(30), running)
            .await
            .expect("deadline")
            .expect("joined");
        assert!(status.is_server_error(), "drained, not cancelled: {status}");
        while spin.in_flight() != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "permit never came back"
            );
            tokio::task::yield_now().await;
        }
        assert_eq!(get(&app, "/healthz").await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_real_router_serves_a_plugin_mounted_after_it_was_built() {
        let mounts = PluginMounts::new();
        let state = AppState::for_test();
        state.insert_extension(mounts.clone());
        let router = crate::router::try_build_router_inner(
            Vec::new(),
            &crate::config::AutumnConfig::default(),
            state,
            crate::router::RouterContext {
                exception_filters: Vec::new(),
                scoped_groups: Vec::new(),
                merge_routers: Vec::new(),
                nest_routers: Vec::new(),
                declared_routes: Vec::new(),
                custom_layers: Vec::new(),
                static_gate_layers: Vec::new(),
                #[cfg(feature = "maud")]
                error_page_renderer: None,
                session_store: None,
                #[cfg(feature = "openapi")]
                openapi: None,
                #[cfg(feature = "mcp")]
                mcp: None,
                #[cfg(feature = "webmcp")]
                webmcp: None,
            },
        )
        .expect("router builds");
        assert_eq!(get(&router, "/hello/greet").await.0, StatusCode::NOT_FOUND);
        mounts.mount(&hello("/hello")).expect("mounts");
        assert_eq!(get(&router, "/hello/greet").await.0, StatusCode::OK);
        let _ = mounts.unmount("/hello");
        assert_eq!(get(&router, "/hello/greet").await.0, StatusCode::NOT_FOUND);
    }

    /// A mounted plugin wins over a generated AEO document at its path, as an
    /// application route does: both are served from the router's fallback.
    #[tokio::test]
    async fn a_mounted_plugin_wins_over_a_generated_document() {
        let mounts = PluginMounts::new();
        let state = AppState::for_test();
        state.insert_extension(mounts.clone());
        let router = crate::router::try_build_router_inner(
            Vec::new(),
            &crate::config::AutumnConfig::default(),
            state,
            crate::router::RouterContext {
                exception_filters: Vec::new(),
                scoped_groups: Vec::new(),
                merge_routers: Vec::new(),
                nest_routers: Vec::new(),
                declared_routes: Vec::new(),
                custom_layers: Vec::new(),
                static_gate_layers: Vec::new(),
                #[cfg(feature = "maud")]
                error_page_renderer: None,
                session_store: None,
                #[cfg(feature = "openapi")]
                openapi: None,
                #[cfg(feature = "mcp")]
                mcp: None,
                #[cfg(feature = "webmcp")]
                webmcp: None,
            },
        )
        .expect("router builds");
        let (status, body) = get(&router, "/llms.txt").await;
        assert_eq!(status, StatusCode::OK, "the generated document");
        assert!(body.starts_with("# "), "{body}");
        mounts.mount(&hello("/llms.txt")).expect("mounts");
        assert_eq!(get(&router, "/llms.txt/greet").await.0, StatusCode::OK);
        assert_eq!(
            get(&router, "/llms.txt").await.0,
            StatusCode::NOT_FOUND,
            "the plugin owns the path now"
        );
    }

    #[tokio::test]
    async fn an_undeclared_path_under_a_prefix_gets_the_framework_404() {
        let mounts = PluginMounts::new();
        let app = app(&mounts);
        mounts.mount(&hello("/hello")).expect("mounts");
        let (status, body) = get(&app, "/hello/missing").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("No route matches /hello/missing"), "{body}");
    }

    #[tokio::test]
    async fn a_mounted_response_names_its_route_for_logs_and_metrics() {
        let mounts = PluginMounts::new();
        let app = app(&mounts);
        mounts.mount(&hello("/hello")).expect("mounts");
        let request = HttpRequest::builder()
            .uri("/hello/greet")
            .body(Body::empty())
            .expect("request");
        let response = app.oneshot(request).await.expect("infallible");
        let route = response
            .extensions()
            .get::<crate::middleware::access_log::ResponseRoute>()
            .expect("the route rides on the response");
        assert_eq!(route.0, "/hello/greet");
    }
}
