//! H8: serve files that a job writes at runtime, with a nested raw router.
//!
//! A nightly job writes CSV exports to a folder, often a mounted volume. Each
//! file needs a stable URL that a spreadsheet can open. Autumn serves two
//! kinds of files, and neither fits:
//!
//! - `static/` holds build assets. `autumn build --embed` puts the folder
//!   into the binary, so a file written at runtime never appears there.
//! - With the `storage` feature, `/_blobs` serves files that the app wrote
//!   through `BlobStore::put`. Their URLs are signed and expire. Another
//!   process cannot add files.
//!
//! `tower_http::services::ServeDir` serves a folder, with ranges and ETags.
//! It is a tower service, not a handler. A raw `axum::Router` mounts it, and
//! `nest` puts that router under `/exports`. `declare_plugin_routes` lists
//! its route, so `autumn routes audit` can see it. With `merge`, the audit
//! counts the router as unknown and fails.
//!
//! The router refuses:
//!
//! - paths that climb out of the folder (`..`). `ServeDir` does this.
//! - names that start with a dot, such as a job's `.stock.csv.tmp`.
//!
//! `ServeDir` follows symbolic links. Put only public files in the folder.

use std::path::{Path, PathBuf};

use autumn_web::AppState;
use autumn_web::config::{Env as _, OsEnv};
use autumn_web::route_listing::{RouteClassification, RouteInfo};
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tower_http::services::ServeDir;

/// Where the files are served.
pub const PREFIX: &str = "/exports";

/// The env var that names the exports folder.
pub const DIR_ENV: &str = "STOCKROOM_EXPORTS_DIR";

/// The exports folder: [`DIR_ENV`] if set. Otherwise `exports` in the
/// project folder, which Autumn finds the same way it finds `static/`.
#[must_use]
pub fn dir_from_env() -> PathBuf {
    dir_from(std::env::var_os(DIR_ENV))
}

/// The exports folder for a value of [`DIR_ENV`]. An empty value counts as
/// unset: an empty path serves the process's working folder.
#[must_use]
pub fn dir_from(value: Option<std::ffi::OsString>) -> PathBuf {
    if let Some(dir) = value.filter(|dir| !dir.is_empty()) {
        return dir.into();
    }
    OsEnv.var("AUTUMN_MANIFEST_DIR").map_or_else(
        |_| PathBuf::from("exports"),
        |dir| Path::new(&dir).join("exports"),
    )
}

/// The router for the exports folder. Mount it with `nest(PREFIX, ..)`.
///
/// The route is explicit. Autumn sets its own 404 fallback on each nested
/// router, and that fallback replaces a `fallback_service`.
pub fn router(dir: &Path) -> axum::Router<AppState> {
    let files = ServeDir::new(dir).redirect_path_prefix(PREFIX);
    axum::Router::new()
        .route_service("/{*path}", files)
        .layer(axum::middleware::from_fn(refuse_dotfiles))
}

/// Answer 404 for a path with a segment that starts with a dot.
async fn refuse_dotfiles(request: Request, next: Next) -> Response {
    let hidden = request.uri().path().split('/').any(|segment| {
        segment.starts_with('.') || segment.starts_with("%2e") || segment.starts_with("%2E")
    });
    if hidden {
        StatusCode::NOT_FOUND.into_response()
    } else {
        next.run(request).await
    }
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
