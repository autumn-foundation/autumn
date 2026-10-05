//! H8: serve files that a job writes at runtime, with a nested raw router.
//!
//! A nightly job writes CSV exports to a folder, often a mounted volume.
//! Autumn serves one folder, `static/`, for build assets. `autumn build
//! --embed` puts that folder into the binary, so a file written at runtime
//! never appears there. Export files need their own mount.
//!
//! `tower_http::services::ServeDir` is a tower service, not a handler, so
//! `#[get]` cannot mount it. A raw `axum::Router` can. `nest` mounts it under
//! `/exports`. `declare_plugin_routes` lists its route, so `autumn routes
//! audit` can see it. (`merge` would hide it from the audit.)
//!
//! `ServeDir` refuses paths that climb out of the folder (`..`).

use std::path::{Path, PathBuf};

use autumn_web::AppState;
use autumn_web::config::{Env as _, OsEnv};
use autumn_web::route_listing::{RouteClassification, RouteInfo};
use tower_http::services::ServeDir;

/// Where the files are served.
pub const PREFIX: &str = "/exports";

/// The env var that names the exports folder.
pub const DIR_ENV: &str = "STOCKROOM_EXPORTS_DIR";

/// The exports folder: [`DIR_ENV`] if set. Otherwise `exports` in the
/// project folder, which Autumn finds the same way it finds `static/`.
#[must_use]
pub fn dir_from_env() -> PathBuf {
    if let Some(dir) = std::env::var_os(DIR_ENV) {
        return dir.into();
    }
    OsEnv.var("AUTUMN_MANIFEST_DIR").map_or_else(
        |_| PathBuf::from("exports"),
        |dir| Path::new(&dir).join("exports"),
    )
}

/// The router for the exports folder. Mount it with `nest(PREFIX, ..)`.
///
/// The route is explicit. A nested router's fallback does not run, so
/// `fallback_service` would answer 404 for every file.
pub fn router(dir: &Path) -> axum::Router<AppState> {
    axum::Router::new().route_service("/{*path}", ServeDir::new(dir))
}

/// The route that [`router`] serves, for the route listing and the audit.
#[must_use]
pub fn routes() -> Vec<RouteInfo> {
    vec![RouteInfo {
        method: "GET".to_owned(),
        path: format!("{PREFIX}/{{*path}}"),
        handler: "tower_http::services::ServeDir".to_owned(),
        classification: RouteClassification::Public,
        ..Default::default()
    }]
}
