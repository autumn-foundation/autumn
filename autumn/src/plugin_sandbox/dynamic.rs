//! Mount and unmount a sandboxed plugin on a running app (issue #2433).
//!
//! The served router is not swapped. Mounted plugins sit in a table that the
//! app's 404 fallback reads. A request that matches a static route never
//! touches the table, so ordinary routes pay nothing.
//!
//! # Rules
//!
//! - A static route wins over a mounted prefix. Mount a prefix that no static
//!   route uses.
//! - A prefix is held by one plugin. A prefix inside, or around, a held prefix
//!   is refused.
//! - **Unmount drains.** New requests get 404 at once. A request already
//!   running finishes and keeps its concurrency permit until it ends.
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

use std::fmt;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::response::Response;

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
    /// Counts up with each mount. A re-mounted prefix gets a new number.
    pub generation: u64,
    /// Requests running now.
    pub in_flight: usize,
}

/// Why a plugin could not be mounted.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MountError {
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
            Self::PrefixTaken { prefix, held_by } => {
                write!(f, "prefix {prefix} overlaps a prefix held by {held_by}")
            }
        }
    }
}

impl std::error::Error for MountError {}

/// The table of plugins mounted on a running app. Clones share one table.
#[derive(Clone, Default)]
pub struct PluginMounts {
    _table: Arc<()>,
}

impl PluginMounts {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mount a plugin at its manifest prefix. Returns its generation.
    ///
    /// # Errors
    ///
    /// [`MountError::PrefixTaken`] if the prefix overlaps a held one.
    pub fn mount(&self, _plugin: &SandboxedPlugin) -> Result<u64, MountError> {
        todo!()
    }

    /// Unmount the plugin at `prefix`. New requests get 404 at once.
    /// Running requests finish.
    #[must_use]
    pub fn unmount(&self, _prefix: &str) -> Option<MountedPluginInfo> {
        todo!()
    }

    /// What is mounted now, ordered by prefix.
    #[must_use]
    pub fn list(&self) -> Vec<MountedPluginInfo> {
        todo!()
    }

    /// The routes of every mounted plugin.
    #[must_use]
    pub fn routes(&self) -> Vec<RouteInfo> {
        todo!()
    }

    fn router_for(&self, _path: &str) -> Option<axum::Router> {
        todo!()
    }
}

impl fmt::Debug for PluginMounts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginMounts").finish_non_exhaustive()
    }
}

/// The app's 404 fallback. Serves a mounted plugin, else the usual 404.
pub async fn mounts_fallback(State(_state): State<AppState>, _request: Request) -> Response {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_sandbox::host::SandboxHost;
    use crate::plugin_sandbox::manifest::{ResourceLimits, SandboxManifest};
    use crate::plugin_sandbox::test_guests as guests;
    use axum::body::Body;
    use http::{Request as HttpRequest, StatusCode};
    use tower::ServiceExt as _;

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
        assert_eq!(get(&app, "/hellothere/greet").await.0, StatusCode::NOT_FOUND);
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
        tokio::task::yield_now().await;
        let _ = mounts.unmount("/spin");
        let status = tokio::time::timeout(std::time::Duration::from_secs(30), running)
            .await
            .expect("deadline")
            .expect("joined");
        assert_ne!(status, StatusCode::NOT_FOUND, "drained, not cancelled");
        assert_eq!(spin.in_flight(), 0);
        assert_eq!(get(&app, "/healthz").await.0, StatusCode::OK);
    }
}
